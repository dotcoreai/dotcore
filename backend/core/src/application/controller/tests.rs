use std::{
    collections::BTreeSet,
    fs,
    net::Ipv4Addr,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::UNIX_EPOCH,
};

use dotcore_catalog::{CatalogBuilder, OciArtifactPolicy};
use dotcore_domain::test_support::oci::{ServiceArtifactOci as _, digest_artifact};
use dotcore_domain::test_support::tracing::{Record, capture_records_async};
use dotcore_domain::timing::{ReconcileCadence, ReconciliationState, Timing};
use dotcore_proto::dotcore::application::v1::ApplicationConfigurationAttachment;
use dotcore_proto::dotcore::{
    application::private::v1::{ApplicationLifecycleObservation, ApplicationStatusStage},
    application::v1::{
        ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationEndpoint, ApplicationLifecycleState,
        ApplicationReadinessState, ApplicationSelectedInferenceRoute,
        ApplicationServiceAcceleratorMemory, ApplicationServiceLifecycleState,
        ApplicationServiceObservation, ApplicationStorageAttachment,
        CreateApplicationDeploymentRequest, CurrentApplicationDeployment,
        UpdateApplicationDeploymentMetadataRequest,
        application_deployment_service_server::ApplicationDeploymentService as ApplicationServiceContract,
    },
    catalog::v1::{
        ApplicationConfigurationBinding, ApplicationDependencyCondition, ApplicationHttpReadiness,
        ApplicationInferenceRouteProjection, ApplicationSecretBinding, ApplicationService,
        ApplicationServiceDependency, ApplicationServicePort, ApplicationServiceReadiness,
        ParameterValue, StorageBinding, StorageEnvironmentVariable, StorageKind, StorageScope,
        application_configuration_binding, application_service_readiness, parameter_value,
    },
    deployment::v1::{
        AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation, AcceleratorMemoryUnavailable,
        AcceleratorMemoryUsageSet, ArtifactPreparationPolicy, ExactPlacement,
        accelerator_memory_observation,
    },
    site::v1::{
        ApplicationRuntimeBinding, DockerApplicationRuntimeAdapter, ExecutionTarget,
        application_runtime_binding, execution_target,
    },
};
use dotcore_runtime::{
    ObservedWorkloadGroup, RuntimeBackendError, WorkloadGroupId, WorkloadGroupObservation,
    WorkloadUnitName, WorkloadUnitState,
};
use dotcore_site::{
    node::target_inventory::{
        RuntimePlatformObservation, StaticTargetCapabilitySource, TargetCapabilityInventory,
        TargetCapabilityReconciler,
    },
    repository::{SiteRepository, SiteRepositoryError, SiteWriteFault},
    store::{FileCurrentState, InMemoryCurrentState},
    view::{contract::application_cleanup_evidence, fencing::NodeIncarnation},
};
use tokio::{io::AsyncWriteExt, sync::Notify};
use tonic::Request;

use super::identity::allocate_deployment_id;
use super::launch::configuration_attachments;
use super::launch::{deployment_data, workload_group_launch};
use super::observation::{exact_owned_observation, stale_owned_group};
use super::readiness::{
    ENDPOINT_NOT_ASSIGNED_STAGE, ENDPOINT_NOT_READY_STAGE, GROUP_NOT_RUNNING_STAGE,
    awaits_readiness, primary_endpoint_ready,
};
use super::realizer::wait_for_observer_wakeup;
use super::test_support::*;
use super::*;
use crate::dispatch::application::{ApplicationRuntimeError, TargetWorkloadGroupObservation};

#[tokio::test]
async fn application_artifact_verification_failure_prevents_runtime_creation_with_a_public_failure()
{
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    desired
        .upsert(deployment(ApplicationDeploymentDesiredState::Present), 0)
        .expect("desired deployment");
    let runtime_calls = Arc::new(Mutex::new(Vec::new()));
    let preparer = Arc::new(FailingApplicationPreparer::default());
    let artifacts =
        dotcore_artifact::ArtifactManager::new(directory.join("artifacts"), Some(preparer.clone()))
            .expect("Artifact manager");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            SelectiveCreateRuntime {
                failures: Arc::new(Mutex::new(BTreeSet::new())),
                missing_images: Arc::new(Mutex::new(BTreeSet::new())),
                taken_ports: Arc::new(Mutex::new(BTreeSet::new())),
                calls: Arc::clone(&runtime_calls),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new_with_artifact_manager(
        test_catalog(),
        desired,
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
        artifacts,
    )
    .expect("Apps controller");

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert_eq!(preparer.requests().len(), 1);
    assert_eq!(
        *runtime_calls.lock().expect("Runtime calls"),
        Vec::<String>::new()
    );
    let state = current.snapshot().expect("current state").deployments[0]
        .state
        .as_ref()
        .expect("failed state")
        .clone();
    assert_eq!(state.stage, "artifact-preparation");
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );
    assert_eq!(
        state.failure,
        Some(ApplicationDeploymentFailure {
            code: "ArtifactPreparationFailed".to_owned(),
            message: "Application image did not match its pinned identity or this node's platform"
                .to_owned(),
            diagnostics: Vec::new(),
        })
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("retried reconciliation");

    assert_eq!(preparer.requests().len(), 2);
    let retried = current.snapshot().expect("current state").deployments[0]
        .state
        .as_ref()
        .expect("failed state")
        .clone();
    assert_eq!(retried.stage, "artifact-preparation");
    assert_eq!(retried.failure, state.failure);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn controller_shutdown_cancels_stalled_application_preparation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    desired
        .upsert(deployment(ApplicationDeploymentDesiredState::Present), 0)
        .expect("desired deployment");
    let preparer = Arc::new(StallingApplicationPreparer::default());
    let artifacts =
        dotcore_artifact::ArtifactManager::new(directory.join("artifacts"), Some(preparer.clone()))
            .expect("Artifact manager");
    let controller = controller_with_runtime_and_artifacts(
        desired,
        current,
        SelectiveCreateRuntime {
            failures: Arc::new(Mutex::new(BTreeSet::new())),
            missing_images: Arc::new(Mutex::new(BTreeSet::new())),
            taken_ports: Arc::new(Mutex::new(BTreeSet::new())),
            calls: Arc::new(Mutex::new(Vec::new())),
        },
        artifacts,
    );
    let inner = Arc::clone(&controller.realizer);
    let reconciliation = tokio::spawn(async move { inner.reconcile().await });
    preparer.started.notified().await;

    drop(controller);

    tokio::time::timeout(Duration::from_secs(1), preparer.cancelled.notified())
        .await
        .expect("shutdown cancellation");
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("cancelled reconciliation is observed");
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn observer_starts_once_and_each_desired_change_wakes_reconciliation() {
    let desired = TestApplicationDeclarationRepository::open(Path::new("."), "site-one")
        .expect("declaration repository");
    let runtime = ObserverRuntime::new();
    let controller = controller_with_runtime(
        desired.clone(),
        TestCurrentApplicationRepository::open(Path::new("."), "site-one")
            .expect("current repository"),
        runtime.clone(),
    );

    let initial = runtime.next_discovery();
    let first = controller.clone();
    let second = controller.clone();
    tokio::join!(async move { first.start_observer() }, async move {
        second.start_observer()
    });
    initial.await;
    controller.start_observer();
    tokio::task::yield_now().await;
    assert_eq!(runtime.discovery_count(), 1);

    let desired_wakeup = runtime.next_discovery();
    desired.listener().notify_one();
    desired_wakeup.await;
    assert_eq!(runtime.discovery_count(), 2);

    drop(controller);
}

#[tokio::test(start_paused = true)]
async fn observer_wakeup_waits_for_the_thirty_second_cadence() {
    let desired = Notify::new();
    let mut cadence = ReconcileCadence::new(Timing::production().application_reconcile);
    let mut wakeup = Box::pin(wait_for_observer_wakeup(
        &desired,
        cadence.next_delay(ReconciliationState::Settled),
    ));

    assert_future_blocks(wakeup.as_mut()).await;

    tokio::time::advance(Duration::from_secs(29)).await;
    assert_future_blocks(wakeup.as_mut()).await;

    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(!wakeup.await);
}

#[tokio::test(start_paused = true)]
async fn observer_wakeup_retries_a_pending_endpoint_within_a_quarter_second() {
    let desired = Notify::new();
    let mut cadence = ReconcileCadence::new(Timing::production().application_reconcile);
    let mut wakeup = Box::pin(wait_for_observer_wakeup(
        &desired,
        cadence.next_delay(ReconciliationState::Pending),
    ));

    assert_future_blocks(wakeup.as_mut()).await;

    tokio::time::advance(Duration::from_millis(250)).await;
    assert!(!wakeup.await);
}

#[tokio::test(start_paused = true)]
async fn observer_wakeup_reports_an_intent_change() {
    let desired = Notify::new();
    desired.notify_one();

    assert!(wait_for_observer_wakeup(&desired, Duration::from_secs(30)).await);
}

#[test]
fn only_a_booting_group_without_a_failure_awaits_readiness() {
    let endpoint = ApplicationEndpoint {
        url: "http://127.0.0.1:41000".to_owned(),
    };
    for stage in [
        GROUP_NOT_RUNNING_STAGE,
        ENDPOINT_NOT_ASSIGNED_STAGE,
        ENDPOINT_NOT_READY_STAGE,
    ] {
        let unavailable = unavailable_state(stage, Some(endpoint.clone()));
        assert!(awaits_readiness(&unavailable), "{stage}");
        assert!(
            awaits_readiness(&booting_state(unavailable.clone())),
            "{stage}"
        );

        let mut failed = unavailable;
        failed.failure = Some(ApplicationDeploymentFailure {
            code: "CreationFailed".to_owned(),
            message: "Application workload creation failed".to_owned(),
            diagnostics: Vec::new(),
        });
        assert!(!awaits_readiness(&failed), "{stage}");
    }

    let mut creation_failure = starting_state(None);
    creation_failure.failure = Some(ApplicationDeploymentFailure {
        code: "ImageMissing".to_owned(),
        message: "Application image is not available locally".to_owned(),
        diagnostics: Vec::new(),
    });
    assert!(!awaits_readiness(&creation_failure));
    assert!(!awaits_readiness(&unavailable_state(
        "Application runtime group is stale or inconclusive",
        None
    )));
    assert!(!awaits_readiness(&deployment_failure_state(
        "readiness-operation",
        None
    )));
}

#[tokio::test]
async fn observer_releases_controller_dependencies_after_the_final_handle_drops() {
    let desired = TestApplicationDeclarationRepository::open(Path::new("."), "site-one")
        .expect("declaration repository");
    let dropped = Arc::new(Notify::new());
    let runtime = ObserverRuntime::with_drop_probe(Arc::clone(&dropped));
    let discoveries = runtime.discovery_listener();
    let controller = controller_with_runtime(
        desired,
        TestCurrentApplicationRepository::open(Path::new("."), "site-one")
            .expect("current repository"),
        runtime,
    );

    let initial = discoveries.notified();
    controller.start_observer();
    initial.await;
    let released = dropped.notified();
    drop(controller);
    released.await;
}

#[tokio::test]
async fn public_application_metadata_update_reports_uncertain_after_memory_acceptance() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .upsert(deployment.clone(), 0)
        .expect("present desired deployment");
    let current_before = current.snapshot().expect("current state before metadata");
    desired.fail_next_upsert(TestDeclarationWriteFailure::OutcomeUncertain);
    let controller = owner_controller(desired.clone(), current.clone());
    let service = crate::application::service::ApplicationService::new(
        controller,
        crate::management_logs::ManagementLogAdmission::new(),
    );

    let error = ApplicationServiceContract::update_application_deployment_metadata(
        &service,
        Request::new(UpdateApplicationDeploymentMetadataRequest {
            application_deployment_id: deployment.application_deployment_id.clone(),
            metadata: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                    display_name: Some("accepted in memory".to_owned()),
                },
            ),
            update_mask: Some(dotcore_proto::google::protobuf::FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        }),
    )
    .await
    .expect_err("uncertain desired persistence");
    assert_eq!(error.code(), tonic::Code::Unavailable);
    assert_eq!(
        error.message(),
        "Application lifecycle outcome is uncertain. Retry the operation or inspect current state."
    );

    let desired_after = desired.snapshot().expect("authoritative desired memory");
    assert_eq!(
        desired_after
            .deployments
            .iter()
            .find(|value| value.application_deployment_id == deployment.application_deployment_id)
            .expect("accepted deployment")
            .application_deployment_name,
        "accepted in memory"
    );
    assert_eq!(
        current.snapshot().expect("current state after metadata"),
        current_before
    );
    drop(service);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn paused_reconciliation_exhaustively_maps_exact_unit_states_without_unsafe_actions() {
    for (unit_state, lifecycle, stage, expected_calls) in [
        (
            WorkloadUnitState::Starting,
            ApplicationLifecycleState::Pausing,
            "pausing",
            vec!["stop"],
        ),
        (
            WorkloadUnitState::Running,
            ApplicationLifecycleState::Pausing,
            "pausing",
            vec!["stop"],
        ),
        (
            WorkloadUnitState::Stopping,
            ApplicationLifecycleState::Pausing,
            "pausing",
            Vec::new(),
        ),
        (
            WorkloadUnitState::Stopped,
            ApplicationLifecycleState::Paused,
            "paused",
            Vec::new(),
        ),
        (
            WorkloadUnitState::Unavailable,
            ApplicationLifecycleState::Pausing,
            "pausing",
            vec!["stop"],
        ),
        (
            WorkloadUnitState::Absent,
            ApplicationLifecycleState::Unavailable,
            "Application runtime group is unavailable while paused",
            Vec::new(),
        ),
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        let launch = workload_group_launch(
            &deployment,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("launch");
        let observations = vec![group_observation(&launch, [("web", unit_state)], false)];
        let calls = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_runtime(
            desired,
            current.clone(),
            PausedMatrixRuntime::new(observations, None, Arc::clone(&calls)),
        );

        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");

        let state = current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("reconstructed state");
        assert_suppressed(&state, lifecycle);
        assert_eq!(state.stage, stage);
        assert_eq!(*calls.lock().expect("Runtime calls"), expected_calls);
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn paused_reconciliation_stops_only_exact_owned_active_partial_or_stale_groups() {
    for (partial, stale, unit_state, lifecycle, expected_calls) in [
        (
            true,
            false,
            WorkloadUnitState::Running,
            ApplicationLifecycleState::Pausing,
            vec!["stop"],
        ),
        (
            true,
            false,
            WorkloadUnitState::Stopped,
            ApplicationLifecycleState::Unavailable,
            Vec::new(),
        ),
        (
            true,
            false,
            WorkloadUnitState::Stopping,
            ApplicationLifecycleState::Pausing,
            Vec::new(),
        ),
        (
            true,
            false,
            WorkloadUnitState::Unavailable,
            ApplicationLifecycleState::Pausing,
            vec!["stop"],
        ),
        (
            true,
            false,
            WorkloadUnitState::Absent,
            ApplicationLifecycleState::Unavailable,
            Vec::new(),
        ),
        (
            false,
            true,
            WorkloadUnitState::Running,
            ApplicationLifecycleState::Pausing,
            vec!["stop"],
        ),
        (
            false,
            true,
            WorkloadUnitState::Stopped,
            ApplicationLifecycleState::Unavailable,
            Vec::new(),
        ),
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let mut deployment = deployment(ApplicationDeploymentDesiredState::Paused);
        if partial {
            deployment
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services
                .push(ApplicationService {
                    service_name: "node".to_owned(),
                    artifact: Some(digest_artifact(
                        Some("example.test"),
                        "node",
                        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        None,
                    )),
                    command: vec!["node".to_owned()],
                    ..Default::default()
                });
        }
        synchronize_service_selections(&mut deployment);
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        let launch = workload_group_launch(
            &deployment,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("launch");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_runtime(
            desired,
            current.clone(),
            PausedMatrixRuntime::new(
                vec![group_observation(&launch, [("web", unit_state)], stale)],
                None,
                Arc::clone(&calls),
            ),
        );

        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");

        let state = current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("reconstructed state");
        assert_suppressed(&state, lifecycle);
        assert_eq!(*calls.lock().expect("Runtime calls"), expected_calls);
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn paused_unavailable_requires_a_later_conclusive_observation_before_paused() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    desired
        .upsert(deployment.clone(), 0)
        .expect("paused desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let first = controller_with_runtime(
        desired.clone(),
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Unavailable)],
                false,
            )],
            None,
            Arc::clone(&first_calls),
        ),
    );

    first
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation");

    let state = current.snapshot().expect("first current state").deployments[0]
        .state
        .clone()
        .expect("first state");
    assert_suppressed(&state, ApplicationLifecycleState::Pausing);
    assert_eq!(*first_calls.lock().expect("first runtime calls"), ["stop"]);
    drop(first);

    let second_calls = Arc::new(Mutex::new(Vec::new()));
    let second = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                false,
            )],
            None,
            Arc::clone(&second_calls),
        ),
    );

    second
        .realizer
        .reconcile()
        .await
        .expect("later reconciliation");

    let state = current.snapshot().expect("later current state").deployments[0]
        .state
        .clone()
        .expect("later state");
    assert_suppressed(&state, ApplicationLifecycleState::Paused);
    assert!(second_calls.lock().expect("later runtime calls").is_empty());
    drop(second);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn paused_partial_unavailable_remains_pausing_until_no_verified_active_or_unavailable_unit() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    deployment
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services
        .push(ApplicationService {
            service_name: "node".to_owned(),
            artifact: Some(digest_artifact(
                Some("example.test"),
                "node",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                None,
            )),
            command: vec!["node".to_owned()],
            ..Default::default()
        });
    synchronize_service_selections(&mut deployment);
    desired
        .upsert(deployment.clone(), 0)
        .expect("paused desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let first = controller_with_runtime(
        desired.clone(),
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Unavailable)],
                false,
            )],
            None,
            Arc::clone(&first_calls),
        ),
    );

    first
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation");

    let state = current.snapshot().expect("first current state").deployments[0]
        .state
        .clone()
        .expect("first state");
    assert_suppressed(&state, ApplicationLifecycleState::Pausing);
    assert_eq!(*first_calls.lock().expect("first runtime calls"), ["stop"]);
    drop(first);

    let second_calls = Arc::new(Mutex::new(Vec::new()));
    let second = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                false,
            )],
            None,
            Arc::clone(&second_calls),
        ),
    );

    second
        .realizer
        .reconcile()
        .await
        .expect("later reconciliation");

    let state = current.snapshot().expect("later current state").deployments[0]
        .state
        .clone()
        .expect("later state");
    assert_suppressed(&state, ApplicationLifecycleState::Unavailable);
    assert_eq!(
        state.stage,
        "Application runtime group is unavailable while paused"
    );
    assert!(second_calls.lock().expect("later runtime calls").is_empty());
    drop(second);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn repeated_paused_reconciliation_only_retries_the_idempotent_group_stop() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    desired
        .upsert(deployment.clone(), 0)
        .expect("paused desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let controller = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Running)],
                false,
            )],
            None,
            Arc::clone(&calls),
        ),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation");
    controller
        .realizer
        .reconcile()
        .await
        .expect("repeat reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["stop", "stop"]);
    let state = current.snapshot().expect("current state").deployments[0]
        .state
        .clone()
        .expect("repeated state");
    assert_suppressed(&state, ApplicationLifecycleState::Pausing);
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn unchanged_reconciliation_keeps_timestamps_and_skips_the_current_state_write() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    desired
        .upsert(deployment.clone(), 0)
        .expect("paused desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let controller = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                false,
            )],
            None,
            Arc::new(Mutex::new(Vec::new())),
        ),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation");
    let first = current.snapshot().expect("first snapshot").deployments[0].clone();
    assert_eq!(
        first.state.as_ref().map(|state| state.lifecycle_state),
        Some(ApplicationLifecycleState::Paused as i32)
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    controller
        .realizer
        .reconcile()
        .await
        .expect("repeat reconciliation");

    let repeated = current.snapshot().expect("repeat snapshot").deployments[0].clone();
    assert_eq!(repeated, first);
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn paused_reconciliation_reports_unsafe_ownership_evidence_without_runtime_mutation() {
    let directory = test_directory();
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let backend_id = dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned());
    let foreign = WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
        correlation: dotcore_runtime::WorkloadCorrelation {
            owner_id: "foreign-deployment".to_owned(),
            reconcile_generation: launch.correlation.reconcile_generation,
        },
        ..observed_group(
            &launch,
            [("web", WorkloadUnitState::Running)],
            backend_id.clone(),
        )
    });
    let mut mismatched_correlation = observed_group(
        &launch,
        [("web", WorkloadUnitState::Running)],
        backend_id.clone(),
    );
    mismatched_correlation.correlation.reconcile_generation += 1;
    let mut mismatched_group_id = observed_group(
        &launch,
        [("web", WorkloadUnitState::Running)],
        backend_id.clone(),
    );
    mismatched_group_id.group_id = WorkloadGroupId("other-group".to_owned());
    let mut mismatched_backend = observed_group(
        &launch,
        [("web", WorkloadUnitState::Running)],
        backend_id.clone(),
    );
    mismatched_backend.backend_id = dotcore_runtime::RuntimeBackendId("runtime/foreign".to_owned());
    let mut unexpected_unit = observed_group(
        &launch,
        [("unexpected", WorkloadUnitState::Running)],
        backend_id.clone(),
    );
    unexpected_unit.units.insert(
        WorkloadUnitName("web".to_owned()),
        dotcore_runtime::WorkloadUnitObservation {
            state: WorkloadUnitState::Running,
            ready_launch_gate: None,
            host_process: None,
        },
    );
    let observations = vec![
        (
            Vec::new(),
            "Application runtime group is unavailable while paused",
        ),
        (
            vec![WorkloadGroupObservation::Absent {
                group_id: launch.group_id.clone(),
                correlation: launch.correlation.clone(),
                backend_id: backend_id.clone(),
            }],
            "Application runtime group is unavailable while paused",
        ),
        (vec![foreign], "runtime-ownership"),
        (
            vec![WorkloadGroupObservation::Observed(mismatched_group_id)],
            "runtime-ownership",
        ),
        (
            vec![WorkloadGroupObservation::Observed(mismatched_correlation)],
            "runtime-ownership",
        ),
        (
            vec![WorkloadGroupObservation::Observed(mismatched_backend)],
            "runtime-ownership",
        ),
        (
            vec![WorkloadGroupObservation::Observed(unexpected_unit)],
            "runtime-ownership",
        ),
        (
            vec![
                group_observation(&launch, [("web", WorkloadUnitState::Running)], false),
                group_observation(&launch, [("web", WorkloadUnitState::Running)], false),
            ],
            "runtime-ownership",
        ),
    ];

    for (observations, expected_stage) in observations {
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_runtime(
            desired,
            current.clone(),
            PausedMatrixRuntime::new(observations, None, Arc::clone(&calls)),
        );

        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");

        let state = current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("safe state reconstruction");
        assert_suppressed(&state, ApplicationLifecycleState::Unavailable);
        assert_eq!(state.stage, expected_stage);
        assert!(calls.lock().expect("Runtime calls").is_empty());
        drop(controller);
    }
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn paused_reconciliation_rehydrates_unknown_discovery_through_inspection() {
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let backend_id = dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned());
    let unknown = WorkloadGroupObservation::Unknown {
        group_id: launch.group_id.clone(),
        backend_id,
    };
    let cases = [
        (
            Some(group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                false,
            )),
            ApplicationLifecycleState::Paused,
            "paused",
            vec!["inspect"],
        ),
        (
            Some(group_observation(
                &launch,
                [("web", WorkloadUnitState::Running)],
                false,
            )),
            ApplicationLifecycleState::Pausing,
            "pausing",
            vec!["inspect", "stop"],
        ),
        (
            Some(unknown.clone()),
            ApplicationLifecycleState::Unavailable,
            "Application runtime group ownership is unverified while paused",
            vec!["inspect"],
        ),
        (
            None,
            ApplicationLifecycleState::Unavailable,
            "runtime-inspection",
            vec!["inspect"],
        ),
    ];

    for (inspection, lifecycle, stage, expected_calls) in cases {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runtime = PausedMatrixRuntime::new(vec![unknown.clone()], None, Arc::clone(&calls));
        let runtime = match inspection {
            Some(inspection) => runtime.with_inspection(inspection),
            None => runtime,
        };
        let controller = controller_with_runtime(desired, current.clone(), runtime);

        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");

        let state = current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("paused state");
        assert_suppressed(&state, lifecycle);
        assert_eq!(state.stage, stage);
        assert_eq!(*calls.lock().expect("Runtime calls"), expected_calls);
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn resume_starts_only_the_complete_retained_group_and_present_recovers_drift() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .upsert(deployment.clone(), 0)
        .expect("present desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let preparer = Arc::new(RecordingApplicationPreparer {
        requests: Mutex::new(Vec::new()),
        calls: Arc::clone(&calls),
    });
    let artifacts =
        dotcore_artifact::ArtifactManager::new(directory.join("artifacts"), Some(preparer.clone()))
            .expect("Artifact manager");
    let controller = controller_with_runtime_and_artifacts(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                false,
            )],
            Some(group_observation(
                &launch,
                [("web", WorkloadUnitState::Running)],
                false,
            )),
            Arc::clone(&calls),
        ),
        artifacts,
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["prepare", "start"]);
    assert_eq!(
        preparer.requests.lock().expect("OCI requests").as_slice(),
        [dotcore_artifact::OciPreparationRequest {
            reference: format!("example.test/open-webui@sha256:{}", "a".repeat(64)),
            platform: dotcore_artifact::OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            },
            source: dotcore_artifact::OciPreparationSource::ImmutableRemote,
        }]
    );
    let state = current.snapshot().expect("current state").deployments[0]
        .state
        .clone()
        .expect("started group state");
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        state.stage,
        "Application primary endpoint is not assigned on the configured host"
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn present_recovery_replaces_stale_groups_only_after_resume_changes_desired_intent() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .upsert(deployment.clone(), 0)
        .expect("present desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let controller = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::with_recovery(
            vec![group_observation(
                &launch,
                [("web", WorkloadUnitState::Stopped)],
                true,
            )],
            group_observation(&launch, [("web", WorkloadUnitState::Running)], false),
            Arc::clone(&calls),
            Arc::new(Mutex::new(Vec::new())),
        ),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["remove", "create"]);
    let state = current.snapshot().expect("current state").deployments[0]
        .state
        .clone()
        .expect("recovered group state");
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        state.stage,
        "Application primary endpoint is not assigned on the configured host"
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn launch_keys_each_storage_binding_by_its_scope() {
    let mut deployment = deployment(ApplicationDeploymentDesiredState::Present);
    let web = &mut deployment
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0];
    web.storage_bindings = vec![
        StorageBinding {
            binding_name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            kind: StorageKind::Data as i32,
            scope: StorageScope::Deployment as i32,
            environment: Vec::new(),
        },
        StorageBinding {
            binding_name: "models".to_owned(),
            mount_path: "/data/models".to_owned(),
            kind: StorageKind::Data as i32,
            scope: StorageScope::Application as i32,
            environment: vec![StorageEnvironmentVariable {
                name: "MODELS_DIR".to_owned(),
                relative_path: "checkpoints".to_owned(),
            }],
        },
    ];
    deployment.storage_attachments = ["data", "models"]
        .map(|binding| ApplicationStorageAttachment {
            service_name: "web".to_owned(),
            binding_name: binding.to_owned(),
            storage_name: format!("web-{binding}"),
        })
        .to_vec();

    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");

    let web = &launch.units[&WorkloadUnitName("web".to_owned())];
    assert_eq!(
        web.storage,
        [
            dotcore_runtime::WorkloadStorageAttachment {
                name: "web-data".to_owned(),
                destination: "/data".to_owned(),
                kind: dotcore_runtime::WorkloadStorageKind::Data,
                namespace: "deployment".to_owned(),
                key: "open-webui-1".to_owned(),
                read_only: false,
                expires_when_unused: false,
            },
            dotcore_runtime::WorkloadStorageAttachment {
                name: "web-models".to_owned(),
                destination: "/data/models".to_owned(),
                kind: dotcore_runtime::WorkloadStorageKind::Data,
                namespace: "application".to_owned(),
                key: "open-webui".to_owned(),
                read_only: false,
                expires_when_unused: true,
            },
        ]
    );
    assert_eq!(
        web.environment.get("MODELS_DIR").map(String::as_str),
        Some("/data/models/checkpoints")
    );
    // Teardown removes exactly the deployment-scoped storage the launch attached.
    assert_eq!(web.storage[0].storage_key(), deployment_data(&deployment));
}

#[test]
fn master_catalog_comfyui_launch_attaches_deployment_data_and_shared_models() {
    let catalog = dotcore_catalog::Catalog::load_with_policy(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../catalog"),
        OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("master catalog");
    let comfyui = catalog
        .list_application_releases()
        .into_iter()
        .find(|application| application.application_id == "comfyui")
        .expect("comfyui application");
    let mut deployment = deployment(ApplicationDeploymentDesiredState::Present);
    deployment.application_deployment_id = "comfyui-1".to_owned();
    deployment.snapshot = Some(resolved_snapshot(comfyui));
    // The launch needs an image it can pin; the storage bindings under test stay the catalog's.
    deployment
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0]
        .artifact = application().graph.expect("graph").services[0]
        .artifact
        .clone();
    synchronize_service_selections(&mut deployment);
    deployment.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "comfyui".to_owned(),
        binding_name: "vram-mode".to_owned(),
        environment_variable: "COMFYUI_VRAM_MODE".to_owned(),
        value: "auto".to_owned(),
    }];
    deployment.storage_attachments = ["data", "models"]
        .map(|binding| ApplicationStorageAttachment {
            service_name: "comfyui".to_owned(),
            binding_name: binding.to_owned(),
            storage_name: format!("comfyui-{binding}"),
        })
        .to_vec();

    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");

    assert_eq!(
        launch.units[&WorkloadUnitName("comfyui".to_owned())].storage,
        [
            dotcore_runtime::WorkloadStorageAttachment {
                name: "comfyui-data".to_owned(),
                destination: "/data".to_owned(),
                kind: dotcore_runtime::WorkloadStorageKind::Data,
                namespace: "deployment".to_owned(),
                key: "comfyui-1".to_owned(),
                read_only: false,
                expires_when_unused: false,
            },
            dotcore_runtime::WorkloadStorageAttachment {
                name: "comfyui-models".to_owned(),
                destination: "/data/models".to_owned(),
                kind: dotcore_runtime::WorkloadStorageKind::Data,
                namespace: "application".to_owned(),
                key: "comfyui".to_owned(),
                read_only: false,
                expires_when_unused: true,
            },
        ]
    );
}

#[tokio::test]
async fn pause_and_resume_operations_are_idempotent_and_clear_public_endpoint() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let group = match running_group(
        &launch,
        dotcore_runtime::RuntimeBackendId("runtime/test".to_owned()),
    ) {
        WorkloadGroupObservation::Observed(group) => group,
        _ => unreachable!("running group helper returns observed group"),
    };
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(ready_state(
                    &group,
                    dotcore_proto::dotcore::application::v1::ApplicationEndpoint {
                        url: "http://127.0.0.1:8080".to_owned(),
                    },
                )),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let controller = owner_controller(desired.clone(), current.clone());

    let paused = controller
        .pause_deployment(deployment.application_deployment_id.clone())
        .await
        .expect("pause acceptance");
    assert_eq!(
        paused.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    let paused_state = paused.state.expect("paused state");
    assert_eq!(
        paused_state.lifecycle_state,
        ApplicationLifecycleState::Pausing as i32
    );
    assert_eq!(
        paused_state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(paused_state.endpoint, None);

    let repeated_pause = controller
        .pause_deployment(deployment.application_deployment_id.clone())
        .await
        .expect("idempotent pause");
    assert_eq!(
        repeated_pause
            .state
            .expect("repeated pause state")
            .lifecycle_state,
        ApplicationLifecycleState::Pausing as i32
    );

    let resumed = controller
        .resume_deployment(deployment.application_deployment_id.clone())
        .await
        .expect("resume acceptance");
    assert_eq!(
        resumed.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    assert_eq!(
        resumed
            .state
            .as_ref()
            .expect("resume state")
            .lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );

    let repeated_resume = controller
        .resume_deployment(deployment.application_deployment_id)
        .await
        .expect("idempotent resume");
    assert_eq!(repeated_resume, resumed);
    assert_eq!(desired.resume_calls(), 1);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn pause_persistence_fault_keeps_authoritative_intent_suppressed_and_retry_is_projection_only()
 {
    for failure in [
        TestPauseFailure::Storage,
        TestPauseFailure::OutcomeUncertain,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(ApplicationDeploymentDesiredState::Present);
        desired
            .upsert(deployment.clone(), 0)
            .expect("desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(stale_ready_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("current deployment");
        let controller = owner_controller(desired.clone(), current.clone());
        desired.fail_next_pause(failure);

        let result = controller
            .pause_deployment(deployment.application_deployment_id.clone())
            .await;

        match failure {
            TestPauseFailure::Storage => assert!(matches!(
                result,
                Err(ApplicationLifecycleError::Configuration(_))
            )),
            TestPauseFailure::OutcomeUncertain => assert!(matches!(
                result,
                Err(ApplicationLifecycleError::OutcomeUncertain(_))
            )),
        }
        assert_eq!(
            desired
                .snapshot()
                .expect("authoritative intent")
                .deployments[0]
                .desired_state,
            ApplicationDeploymentDesiredState::Paused as i32
        );
        let recorded = current.snapshot().expect("pause current state").deployments[0]
            .state
            .clone()
            .expect("pause state");
        assert_suppressed(&recorded, ApplicationLifecycleState::Pausing);

        let listed = controller
            .list_deployments()
            .await
            .expect("safe list after pause fault");
        assert_suppressed(
            listed.deployments[0].state.as_ref().expect("listed state"),
            ApplicationLifecycleState::Pausing,
        );
        let fetched = controller
            .get_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("safe get after pause fault");
        assert_suppressed(
            fetched.state.as_ref().expect("fetched state"),
            ApplicationLifecycleState::Pausing,
        );

        let retried = controller
            .pause_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("pause retry");
        assert_eq!(desired.pause_calls(), 1);
        assert_eq!(retried.state, Some(recorded));
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn resume_desired_persistence_faults_retry_starting_without_replaying_present_intent() {
    for failure in [
        TestResumeFailure::Write,
        TestResumeFailure::Rename,
        TestResumeFailure::Sync,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(stale_paused_state(ApplicationLifecycleState::Paused)),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("paused current deployment");
        let controller = owner_controller(desired.clone(), current.clone());
        desired.fail_next_resume(failure);

        let result = controller
            .resume_deployment(deployment.application_deployment_id.clone())
            .await;

        match failure {
            TestResumeFailure::Write | TestResumeFailure::Rename => assert!(matches!(
                result,
                Err(ApplicationLifecycleError::Configuration(_))
            )),
            TestResumeFailure::Sync => assert!(matches!(
                result,
                Err(ApplicationLifecycleError::OutcomeUncertain(_))
            )),
        }
        assert_eq!(
            desired
                .snapshot()
                .expect("authoritative desired intent")
                .deployments[0]
                .desired_state,
            ApplicationDeploymentDesiredState::Present as i32
        );
        let persisted = current
            .snapshot()
            .expect("paused current state")
            .deployments[0]
            .state
            .clone()
            .expect("paused state");
        assert_eq!(
            persisted.lifecycle_state,
            ApplicationLifecycleState::Paused as i32
        );
        assert_eq!(
            persisted.readiness_state,
            ApplicationReadinessState::Ready as i32
        );

        let listed = controller
            .list_deployments()
            .await
            .expect("safe list after desired persistence fault");
        assert_suppressed(
            listed.deployments[0].state.as_ref().expect("listed state"),
            ApplicationLifecycleState::Starting,
        );

        let retried = controller
            .resume_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("resume retry");
        assert_suppressed(
            retried.state.as_ref().expect("retry state"),
            ApplicationLifecycleState::Starting,
        );
        let recorded = current
            .snapshot()
            .expect("starting current state")
            .deployments[0]
            .clone();
        assert_eq!(recorded.record_revision, 2);
        assert_suppressed(
            recorded.state.as_ref().expect("starting state"),
            ApplicationLifecycleState::Starting,
        );
        assert_eq!(desired.resume_calls(), 1);
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn resume_current_state_fault_retries_starting_without_returning_present_paused() {
    for lifecycle in [
        ApplicationLifecycleState::Pausing,
        ApplicationLifecycleState::Paused,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = FailingCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
        desired
            .upsert(deployment.clone(), 0)
            .expect("paused desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(stale_paused_state(lifecycle)),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("paused current deployment");
        let controller = owner_controller(desired.clone(), current.clone());
        current.fail_next_upsert(TestCurrentWriteFailure::Storage);

        assert!(matches!(
            controller
                .resume_deployment(deployment.application_deployment_id.clone())
                .await,
            Err(ApplicationLifecycleError::OutcomeUncertain(_))
        ));
        assert_eq!(
            desired
                .snapshot()
                .expect("authoritative desired intent")
                .deployments[0]
                .desired_state,
            ApplicationDeploymentDesiredState::Present as i32
        );
        assert_eq!(
            current
                .snapshot()
                .expect("paused current state")
                .deployments[0]
                .state
                .as_ref()
                .expect("paused state")
                .lifecycle_state,
            lifecycle as i32
        );

        let listed = controller
            .list_deployments()
            .await
            .expect("safe list after current state fault");
        assert_suppressed(
            listed.deployments[0].state.as_ref().expect("listed state"),
            ApplicationLifecycleState::Starting,
        );

        let retried = controller
            .resume_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("resume retry");
        assert_suppressed(
            retried.state.as_ref().expect("retry state"),
            ApplicationLifecycleState::Starting,
        );
        let recorded = current
            .snapshot()
            .expect("starting current state")
            .deployments[0]
            .clone();
        assert_eq!(recorded.record_revision, 2);
        assert_suppressed(
            recorded.state.as_ref().expect("starting state"),
            ApplicationLifecycleState::Starting,
        );
        assert_eq!(desired.resume_calls(), 1);
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn lifecycle_intent_holds_over_a_node_status_accepted_right_after_its_read() {
    for operation in ["pause", "resume", "remove"] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = FailingCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let (desired_state, current_state) = match operation {
            "resume" => (
                ApplicationDeploymentDesiredState::Paused,
                stale_paused_state(ApplicationLifecycleState::Paused),
            ),
            _ => (
                ApplicationDeploymentDesiredState::Present,
                stale_ready_state(),
            ),
        };
        let deployment = deployment(desired_state);
        let deployment_id = deployment.application_deployment_id.clone();
        desired
            .upsert(deployment.clone(), 0)
            .expect("desired deployment");
        let record = |state| CurrentApplicationDeployment {
            application_deployment_id: deployment_id.clone(),
            record_revision: 0,
            state: Some(state),
            reconciled_lifecycle_generation: None,
        };
        current
            .upsert(record(current_state.clone()), None)
            .expect("current deployment");
        let controller = owner_controller(desired.clone(), current.clone());
        current.accept_status_after_next_read(&desired, record(current_state));

        let accepted = match operation {
            "pause" => controller.pause_deployment(deployment_id.clone()).await,
            "resume" => controller.resume_deployment(deployment_id.clone()).await,
            _ => controller.remove_deployment(deployment_id.clone()).await,
        };

        accepted.unwrap_or_else(|error| panic!("{operation} intent recorded: {error:?}"));
        // The status was waiting for the declaration guard and lands after the intent.
        assert!(current.accept_pending_status(), "{operation}");
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn application_producers_retain_repository_sources_without_interpolation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    let controller = owner_controller(desired.clone(), current);
    desired.fail_next_pause(TestPauseFailure::Storage);

    let error = controller
        .pause_deployment(deployment.application_deployment_id)
        .await
        .expect_err("injected desired persistence failure");
    assert!(std::error::Error::source(&error).is_some());
    let ApplicationLifecycleError::Configuration(source) = error else {
        panic!("desired persistence failure must be a configuration error");
    };

    assert_eq!(
        source.to_string(),
        "Application desired state persistence failed"
    );
    assert!(matches!(
        source.downcast_ref::<ApplicationRepositoryError>(),
        Some(ApplicationRepositoryError::Storage { .. })
    ));
    assert_eq!(
        source.chain().last().expect("repository leaf").to_string(),
        "injected desired write failure"
    );
    assert!(
        !source
            .to_string()
            .contains("injected desired write failure")
    );

    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn application_producers_retain_runtime_sources() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let controller = controller_with_runtime(desired, current, FailingDiscoveryRuntime);

    let error = controller
        .realizer
        .reconcile()
        .await
        .expect_err("injected runtime discovery failure");
    assert!(std::error::Error::source(&error).is_some());
    let ApplicationLifecycleError::Configuration(source) = error else {
        panic!("Runtime discovery failure must be a configuration error");
    };

    assert_eq!(source.to_string(), "Runtime group discovery failed");
    assert!(source.downcast_ref::<ApplicationRuntimeError>().is_some());
    assert!(matches!(
        source
            .chain()
            .find_map(|error| error.downcast_ref::<RuntimeBackendError>()),
        Some(RuntimeBackendError::Docker(_))
    ));
    assert_eq!(
        source.chain().last().expect("Runtime leaf").to_string(),
        "injected runtime discovery failure"
    );
    assert!(
        !source
            .to_string()
            .contains("injected runtime discovery failure")
    );

    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn pause_current_state_fault_retries_pausing_without_replaying_paused_intent() {
    for failure in [
        TestCurrentWriteFailure::Storage,
        TestCurrentWriteFailure::OutcomeUncertain,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = FailingCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(ApplicationDeploymentDesiredState::Present);
        desired
            .upsert(deployment.clone(), 0)
            .expect("present desired deployment");
        let stale = ApplicationDeploymentState {
            started_at: Some(Timestamp {
                seconds: 101,
                nanos: 102,
            }),
            updated_at: Some(Timestamp {
                seconds: 103,
                nanos: 104,
            }),
            ..stale_ready_state()
        };
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(stale.clone()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("ready current deployment");
        let controller = owner_controller(desired.clone(), current.clone());
        current.fail_next_upsert(failure);

        let result = controller
            .pause_deployment(deployment.application_deployment_id.clone())
            .await;

        let source = match result {
            Err(ApplicationLifecycleError::OutcomeUncertain(source)) => source,
            other => {
                panic!("Pause should report an outcome-uncertain current-state write: {other:?}")
            }
        };
        assert_eq!(
            source.to_string(),
            "Pause accepted application desired state but could not persist current state; retry the operation"
        );
        assert!(source.chain().nth(1).is_some());
        assert_eq!(
            desired
                .snapshot()
                .expect("authoritative paused intent")
                .deployments[0]
                .desired_state,
            ApplicationDeploymentDesiredState::Paused as i32
        );
        let unmodified = current
            .snapshot()
            .expect("unchanged ready current state")
            .deployments[0]
            .clone();
        assert_eq!(unmodified.record_revision, 1);
        assert_eq!(unmodified.state, Some(stale.clone()));

        let retried = controller
            .pause_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("pause retry");

        let repaired = current
            .snapshot()
            .expect("repaired pausing current state")
            .deployments[0]
            .clone();
        assert_eq!(desired.pause_calls(), 1);
        assert_eq!(repaired.record_revision, 2);
        let repaired_state = repaired.state.expect("repaired pausing state");
        assert_suppressed(&repaired_state, ApplicationLifecycleState::Pausing);
        assert_eq!(repaired_state.started_at, stale.started_at);
        assert_eq!(retried.state, Some(repaired_state));
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn paused_pause_retry_is_a_true_current_state_no_op() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    desired
        .upsert(deployment.clone(), 0)
        .expect("paused desired deployment");
    let pausing = pausing_state(None);
    let paused = paused_state(Some(&pausing));
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(paused),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("paused current deployment");
    let controller = owner_controller(desired.clone(), current.clone());
    let before = current
        .snapshot()
        .expect("paused current snapshot")
        .deployments[0]
        .clone();

    let retried = controller
        .pause_deployment(deployment.application_deployment_id)
        .await
        .expect("idempotent paused retry");

    assert_eq!(desired.pause_calls(), 0);
    assert_eq!(retried.state, before.state);
    assert_eq!(
        current
            .snapshot()
            .expect("unchanged paused current state")
            .deployments[0],
        before
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn list_and_get_sanitize_stale_ready_projection_for_paused_and_teardown_intent() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let paused = deployment(ApplicationDeploymentDesiredState::Paused);
    let mut teardown = paused.clone();
    teardown.application_deployment_id = "open-webui-2".to_owned();
    teardown.application_deployment_name = "open-webui-2".to_owned();
    teardown.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
    desired.upsert(paused.clone(), 0).expect("paused desired");
    desired
        .upsert(teardown.clone(), 1)
        .expect("teardown desired");
    for deployment in [&paused, &teardown] {
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(stale_ready_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("stale current deployment");
    }
    let controller = owner_controller(desired, current);

    let listed = controller
        .list_deployments()
        .await
        .expect("safe deployment list");
    assert_suppressed(
        listed.deployments[0]
            .state
            .as_ref()
            .expect("paused list state"),
        ApplicationLifecycleState::Pausing,
    );
    assert_suppressed(
        listed.deployments[1]
            .state
            .as_ref()
            .expect("teardown list state"),
        ApplicationLifecycleState::Stopping,
    );
    let fetched = controller
        .get_deployment(teardown.application_deployment_id)
        .await
        .expect("safe teardown get");
    assert_suppressed(
        fetched.state.as_ref().expect("teardown get state"),
        ApplicationLifecycleState::Stopping,
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn list_omits_teardown_declaration_without_current_record() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let present = deployment(ApplicationDeploymentDesiredState::Present);
    let mut finalized = present.clone();
    finalized.application_deployment_id = "open-webui-2".to_owned();
    finalized.application_deployment_name = "open-webui-2".to_owned();
    finalized.desired_state = ApplicationDeploymentDesiredState::Teardown as i32;
    desired.upsert(present.clone(), 0).expect("present desired");
    desired
        .upsert(finalized.clone(), 1)
        .expect("teardown desired");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: present.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(stale_ready_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("present current deployment");
    let controller = owner_controller(desired, current);

    let listed = controller
        .list_deployments()
        .await
        .expect("list while teardown finalized");
    assert_eq!(
        listed
            .deployments
            .iter()
            .map(|deployment| (
                deployment.application_deployment_id.as_str(),
                deployment.desired_state
            ))
            .collect::<Vec<_>>(),
        vec![(
            present.application_deployment_id.as_str(),
            ApplicationDeploymentDesiredState::Present as i32
        )]
    );
    assert!(matches!(
        controller
            .get_deployment(finalized.application_deployment_id)
            .await,
        Err(ApplicationLifecycleError::NotFound)
    ));
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn list_reports_configuration_error_for_present_declaration_without_current_record() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    desired
        .upsert(deployment(ApplicationDeploymentDesiredState::Present), 0)
        .expect("present desired");
    let controller = owner_controller(desired, current);

    let error = controller
        .list_deployments()
        .await
        .expect_err("missing current record");
    assert!(matches!(error, ApplicationLifecycleError::Configuration(_)));
    assert_eq!(
        error.to_string(),
        "Application lifecycle configuration failed: Application deployment declaration has no current-state record"
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn persisted_paused_site_yaml_is_authoritative_after_controller_restart() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let deployment = deployment(ApplicationDeploymentDesiredState::Present);
    let mut persisted_site = site();
    persisted_site.application_deployments = vec![deployment.clone()];
    fs::write(
        &path,
        dotcore_proto::yaml::to_string(&persisted_site).expect("site YAML"),
    )
    .expect("write site YAML");
    let desired = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    // Startup resync seeds the durable high-water from the persisted generation 1.
    current
        .reserve_lifecycle_generation(&deployment.application_deployment_id)
        .expect("resynced lifecycle high-water");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(stale_ready_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let controller = owner_controller(desired, current.clone());

    controller
        .pause_deployment(deployment.application_deployment_id.clone())
        .await
        .expect("durable pause");
    let revision = current.snapshot().expect("pause state").deployments[0].record_revision;
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(stale_ready_state()),
                reconciled_lifecycle_generation: None,
            },
            Some(revision),
        )
        .expect("stale state after shutdown");
    drop(controller);

    let restarted = owner_controller(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("reloaded site repository"),
        current,
    );
    let deployment = restarted
        .get_deployment(deployment.application_deployment_id)
        .await
        .expect("paused deployment after restart");
    assert_eq!(
        deployment.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    assert_suppressed(
        deployment.state.as_ref().expect("safe restart state"),
        ApplicationLifecycleState::Pausing,
    );
    drop(restarted);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn failed_real_repository_resume_retains_present_memory_while_reload_is_paused() {
    let directory = test_directory();
    let path = directory.join("site.yaml");
    let deployment = deployment(ApplicationDeploymentDesiredState::Paused);
    let mut persisted_site = site();
    persisted_site.application_deployments = vec![deployment.clone()];
    let persisted_contents = dotcore_proto::yaml::to_string(&persisted_site).expect("site YAML");
    fs::write(&path, &persisted_contents).expect("write site YAML");
    let desired = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("desired site repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    // Startup resync seeds the durable high-water from the persisted generation 1.
    current
        .reserve_lifecycle_generation(&deployment.application_deployment_id)
        .expect("resynced lifecycle high-water");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(stale_paused_state(ApplicationLifecycleState::Paused)),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("paused current deployment");
    let controller = owner_controller(desired.clone(), current);
    fs::remove_file(&path).expect("remove site YAML before injected rename failure");
    fs::create_dir(&path).expect("block site YAML replacement");

    assert!(matches!(
        controller
            .resume_deployment(deployment.application_deployment_id.clone())
            .await,
        Err(ApplicationLifecycleError::Configuration(_))
    ));
    assert_eq!(
        ApplicationDeclarationRepository::snapshot(&desired)
            .expect("authoritative desired intent")
            .deployments[0]
            .desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );

    fs::remove_dir(&path).expect("remove replacement blocker");
    fs::write(&path, persisted_contents).expect("restore persisted site YAML");
    drop(controller);
    let reloaded = SiteRepository::load(&path, crate::test_support::site_declaration_rules())
        .expect("reloaded site repository");
    assert_eq!(
        ApplicationDeclarationRepository::snapshot(&reloaded)
            .expect("reloaded desired intent")
            .deployments[0]
            .desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn lifecycle_generations_advance_and_survive_restart() {
    let directory = test_directory();
    let path = write_lifecycle_site(&directory, vec![generation_deployment(1)]);
    let current =
        FileCurrentState::open_at_startup(&path, &crate::test_support::site_declaration_rules())
            .expect("current state");
    assert_eq!(application_high_water(&current), Some(1));
    let controller = owner_controller(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("desired site repository"),
        current.clone(),
    );

    controller
        .pause_deployment("open-webui-1".to_owned())
        .await
        .expect("pause");
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Paused, 2)
    );
    controller
        .pause_deployment("open-webui-1".to_owned())
        .await
        .expect("repeated pause");
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Paused, 2)
    );
    controller
        .resume_deployment("open-webui-1".to_owned())
        .await
        .expect("resume");
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Present, 3)
    );
    controller
        .remove_deployment("open-webui-1".to_owned())
        .await
        .expect("teardown");
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Teardown, 4)
    );
    assert_eq!(application_high_water(&current), Some(4));
    drop(controller);
    drop(current);

    let restarted =
        FileCurrentState::open_at_startup(&path, &crate::test_support::site_declaration_rules())
            .expect("restarted current state");
    assert_eq!(application_high_water(&restarted), Some(4));
    drop(restarted);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn burned_lifecycle_generation_is_never_reused_after_restart() {
    let directory = test_directory();
    let path = write_lifecycle_site(&directory, vec![generation_deployment(1)]);
    let current =
        FileCurrentState::open_at_startup(&path, &crate::test_support::site_declaration_rules())
            .expect("current state");
    let (desired, faults) =
        SiteRepository::load_with_write_faults_for_test(&path).expect("faulting site repository");
    faults.push(SiteWriteFault::Storage);
    let controller = owner_controller(desired, current.clone());

    assert!(
        controller
            .pause_deployment("open-webui-1".to_owned())
            .await
            .is_err()
    );
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Present, 1)
    );
    assert_eq!(application_high_water(&current), Some(2));
    drop(controller);
    drop(current);

    let current =
        FileCurrentState::open_at_startup(&path, &crate::test_support::site_declaration_rules())
            .expect("restarted current state");
    assert_eq!(application_high_water(&current), Some(2));
    let restarted = owner_controller(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("reloaded site repository"),
        current.clone(),
    );
    restarted
        .pause_deployment("open-webui-1".to_owned())
        .await
        .expect("pause after restart");
    assert_eq!(
        declared_lifecycle(&path),
        (ApplicationDeploymentDesiredState::Paused, 3)
    );
    drop(restarted);
    drop(current);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn application_declarations_without_a_lifecycle_generation_are_rejected() {
    let directory = test_directory();
    let path = write_lifecycle_site(&directory, vec![generation_deployment(0)]);

    assert!(matches!(
        SiteRepository::load(&path, crate::test_support::site_declaration_rules()),
        Err(SiteRepositoryError::InvalidSite(_))
    ));
    let mut deployments = Vec::new();
    assert!(matches!(
        crate::application::repository::upsert_declaration(
            &mut deployments,
            generation_deployment(0)
        ),
        Err(ApplicationRepositoryError::InvalidState)
    ));
    assert_eq!(deployments, Vec::new());
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn pause_and_resume_reject_teardown_without_mutation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(stopping_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let controller = owner_controller(desired.clone(), current.clone());

    assert!(matches!(
        controller
            .pause_deployment(deployment.application_deployment_id.clone())
            .await,
        Err(ApplicationLifecycleError::FailedPrecondition)
    ));
    assert!(matches!(
        controller
            .resume_deployment(deployment.application_deployment_id.clone())
            .await,
        Err(ApplicationLifecycleError::FailedPrecondition)
    ));
    assert_eq!(
        desired.snapshot().expect("desired state").deployments[0].desired_state,
        ApplicationDeploymentDesiredState::Teardown as i32
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn unknown_group_after_restart_is_rehydrated_before_present_or_absent_reconciliation() {
    for desired_state in [
        ApplicationDeploymentDesiredState::Present,
        ApplicationDeploymentDesiredState::Teardown,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let mut deployment = deployment(desired_state);
        deployment.storage_attachments = vec![ApplicationStorageAttachment {
            service_name: "web".to_owned(),
            binding_name: "data".to_owned(),
            storage_name: "web-data".to_owned(),
        }];
        desired
            .upsert(deployment.clone(), 0)
            .expect("desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(queued_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("current deployment");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
        runtimes
            .insert(
                "application-target-one",
                RestartRuntime {
                    calls: calls.clone(),
                },
            )
            .expect("Runtime adapter");
        let controller = TestController::new(
            test_catalog(),
            desired,
            current,
            runtimes.build(),
            ApplicationControllerConfiguration {
                target_capabilities: test_target_capability_source(),
                configuration_sources: ApplicationConfigurationSources::default(),
                inference_route_resolver: test_inference_route_resolver(),
            },
        )
        .expect("Apps controller");

        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");
        let calls = calls.lock().expect("Runtime calls").clone();
        if desired_state == ApplicationDeploymentDesiredState::Present {
            assert_eq!(calls, ["inspect"]);
        } else {
            assert_eq!(calls, ["ensure-absent"]);
        }
        std::fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn cold_absent_reconciliation_passes_stable_identity_to_runtime_cleanup() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    let cleanup_identities = Arc::new(Mutex::new(Vec::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            ColdAbsentRuntime {
                cleanup_identities: cleanup_identities.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        test_catalog(),
        desired,
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert_eq!(
        *cleanup_identities.lock().expect("cleanup identities"),
        [(
            WorkloadGroupId(deployment.application_deployment_id.clone()),
            deployment.application_deployment_id,
        )]
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn route_bound_teardown_finalizes_when_route_is_unavailable_and_runtime_is_absent() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    deployment.snapshot = Some(resolved_snapshot(application_with_route_input()));
    deployment.inference_route_id = Some("route-id".to_owned());
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = TeardownSequenceRuntime::new(
        launch.clone(),
        Vec::new(),
        vec![WorkloadGroupObservation::Absent {
            group_id: launch.group_id.clone(),
            correlation: launch.correlation.clone(),
            backend_id: dotcore_runtime::RuntimeBackendId(
                "runtime/teardown-sequence-test".to_owned(),
            ),
        }],
        Arc::clone(&calls),
    );
    let controller = controller_with_runtime_and_resolver(
        desired.clone(),
        current.clone(),
        runtime,
        test_inference_route_resolver_with(Err(InferenceRouteResolutionError::Unavailable)),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("teardown reconciliation");

    let desired_snapshot = desired.snapshot().expect("desired state");
    let current_snapshot = current.snapshot().expect("current state");
    let retained_stage = current_snapshot
        .deployments
        .first()
        .and_then(|record| record.state.as_ref())
        .map(|state| state.stage.as_str());
    assert!(
        desired_snapshot.deployments.is_empty(),
        "confirmed runtime absence must finalize desired teardown; retained stage: {retained_stage:?}; runtime calls: {:?}",
        calls.lock().expect("Runtime calls")
    );
    assert!(
        current_snapshot.deployments.is_empty(),
        "confirmed runtime absence must remove current teardown state"
    );
    assert_eq!(
        *calls.lock().expect("Runtime calls"),
        ["ensure-absent"],
        "teardown must bypass broad discovery and launch-dependent runtime calls"
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn teardown_startup_allows_target_drift_and_retains_missing_runtime_for_retry() {
    let drift_directory = test_directory();
    let drift_desired = TestApplicationDeclarationRepository::open(&drift_directory, "site-one")
        .expect("declaration repository");
    let drift_current = TestCurrentApplicationRepository::open(&drift_directory, "site-one")
        .expect("current repository");
    let mut drifted = deployment(ApplicationDeploymentDesiredState::Teardown);
    drifted
        .snapshot
        .as_mut()
        .expect("snapshot")
        .placement
        .as_mut()
        .expect("snapshot placement")
        .execution_target_id = "retired-target".to_owned();
    drift_desired
        .upsert(drifted.clone(), 0)
        .expect("drifted desired deployment");
    drift_current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: drifted.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("drifted current deployment");
    let cleanup_identities = Arc::new(Mutex::new(Vec::new()));
    let drift_controller = controller_with_runtime_and_resolver(
        drift_desired.clone(),
        drift_current.clone(),
        ColdAbsentRuntime {
            cleanup_identities: Arc::clone(&cleanup_identities),
        },
        test_inference_route_resolver_with(Err(InferenceRouteResolutionError::Unavailable)),
    );
    drift_controller
        .realizer
        .reconcile()
        .await
        .expect("target-drift teardown");
    assert!(
        drift_desired
            .snapshot()
            .expect("drift desired")
            .deployments
            .is_empty()
    );
    assert!(
        drift_current
            .snapshot()
            .expect("drift current")
            .deployments
            .is_empty()
    );
    assert_eq!(
        cleanup_identities.lock().expect("cleanup identities").len(),
        1
    );
    std::fs::remove_dir_all(drift_directory).expect("drift test directory cleanup");

    let missing_directory = test_directory();
    let missing_desired =
        TestApplicationDeclarationRepository::open(&missing_directory, "site-one")
            .expect("declaration repository");
    let missing_current = TestCurrentApplicationRepository::open(&missing_directory, "site-one")
        .expect("current repository");
    let missing = deployment(ApplicationDeploymentDesiredState::Teardown);
    missing_desired
        .upsert(missing.clone(), 0)
        .expect("missing-runtime desired deployment");
    missing_current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: missing.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("missing-runtime current deployment");
    let missing_controller = TestController::new(
        test_catalog(),
        missing_desired.clone(),
        missing_current.clone(),
        crate::dispatch::application::ApplicationRuntimeMapBuilder::new().build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("teardown controller without configured runtime");
    missing_controller
        .realizer
        .reconcile()
        .await
        .expect("missing runtime is a deployment-local retryable failure");
    assert_eq!(
        missing_desired
            .snapshot()
            .expect("retained missing-runtime desired")
            .deployments
            .len(),
        1
    );
    let retained = missing_current
        .snapshot()
        .expect("retained missing-runtime current")
        .deployments
        .remove(0)
        .state
        .expect("retryable cleanup state");
    assert_eq!(
        retained.lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );
    assert_eq!(retained.stage, "runtime-cleanup");
    std::fs::remove_dir_all(missing_directory).expect("missing-runtime test directory cleanup");
}

#[tokio::test]
async fn teardown_retries_exact_network_only_residue_and_finalizes_records() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");
    let removals = Arc::new(Mutex::new(0));
    let creations = Arc::new(Mutex::new(0));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            NetworkResidueRetryRuntime {
                launch,
                removals: Arc::clone(&removals),
                creations,
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        test_catalog(),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    controller
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation records removal failure");
    assert_eq!(*removals.lock().expect("removals"), 1);
    assert_eq!(
        desired
            .snapshot()
            .expect("desired after failure")
            .deployments
            .len(),
        1
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current after failure")
            .deployments
            .len(),
        1
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("network-only retry reconciliation");
    assert_eq!(*removals.lock().expect("removals"), 2);
    assert!(
        desired
            .snapshot()
            .expect("final desired")
            .deployments
            .is_empty()
    );
    assert!(
        current
            .snapshot()
            .expect("final current")
            .deployments
            .is_empty()
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn teardown_retries_after_post_remove_inspection_finds_resources_remaining() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = TeardownSequenceRuntime::new(
        launch.clone(),
        vec![
            teardown_group_observation(&launch, [("web", WorkloadUnitState::Running)]),
            teardown_group_observation(&launch, []),
        ],
        vec![
            teardown_group_observation(&launch, []),
            WorkloadGroupObservation::Absent {
                group_id: launch.group_id.clone(),
                correlation: launch.correlation.clone(),
                backend_id: dotcore_runtime::RuntimeBackendId(
                    "runtime/teardown-sequence-test".to_owned(),
                ),
            },
        ],
        calls.clone(),
    );
    let controller = controller_with_runtime(desired.clone(), current.clone(), runtime);

    controller
        .realizer
        .reconcile()
        .await
        .expect("first reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["ensure-absent"]);
    assert_eq!(
        desired
            .snapshot()
            .expect("desired after first retry")
            .deployments
            .len(),
        1
    );
    assert_eq!(
        desired
            .snapshot()
            .expect("desired state after first retry")
            .deployments[0]
            .desired_state,
        ApplicationDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current after first retry")
            .deployments
            .len(),
        1
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current state after first retry")
            .deployments[0]
            .state
            .as_ref()
            .expect("current state")
            .lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("second reconciliation");

    assert_eq!(
        *calls.lock().expect("Runtime calls"),
        vec!["ensure-absent", "ensure-absent"]
    );
    assert!(
        desired
            .snapshot()
            .expect("final desired")
            .deployments
            .is_empty()
    );
    assert!(
        current
            .snapshot()
            .expect("final current")
            .deployments
            .is_empty()
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn teardown_retains_records_when_post_remove_inspection_is_uncertain_then_retries() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(deployment.clone(), 0)
        .expect("desired deployment");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: deployment.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let runtime = TeardownSequenceRuntime::new(
        launch.clone(),
        vec![teardown_group_observation(
            &launch,
            [("web", WorkloadUnitState::Running)],
        )],
        vec![
            WorkloadGroupObservation::Unknown {
                group_id: launch.group_id.clone(),
                backend_id: dotcore_runtime::RuntimeBackendId(
                    "runtime/teardown-sequence-test".to_owned(),
                ),
            },
            WorkloadGroupObservation::Absent {
                group_id: launch.group_id.clone(),
                correlation: launch.correlation.clone(),
                backend_id: dotcore_runtime::RuntimeBackendId(
                    "runtime/teardown-sequence-test".to_owned(),
                ),
            },
        ],
        calls.clone(),
    );
    let controller = controller_with_runtime(desired.clone(), current.clone(), runtime);

    controller
        .realizer
        .reconcile()
        .await
        .expect("uncertain reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["ensure-absent"]);
    assert_eq!(
        desired
            .snapshot()
            .expect("desired after uncertain inspect")
            .deployments
            .len(),
        1
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current after uncertain inspect")
            .deployments
            .len(),
        1
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current state after uncertain inspect")
            .deployments[0]
            .state
            .as_ref()
            .expect("current state")
            .lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("retry reconciliation");

    assert_eq!(
        *calls.lock().expect("Runtime calls"),
        vec!["ensure-absent", "ensure-absent"]
    );
    assert!(
        desired
            .snapshot()
            .expect("final desired")
            .deployments
            .is_empty()
    );
    assert!(
        current
            .snapshot()
            .expect("final current")
            .deployments
            .is_empty()
    );
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn teardown_confirms_absence_before_deleting_records_for_active_and_paused_deployments() {
    for desired_state in [
        ApplicationDeploymentDesiredState::Present,
        ApplicationDeploymentDesiredState::Paused,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let deployment = deployment(desired_state);
        desired
            .upsert(deployment.clone(), 0)
            .expect("desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(queued_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("current deployment");
        let launch = workload_group_launch(
            &deployment,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("Runtime launch");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let runtime = TeardownSequenceRuntime::new(
            launch.clone(),
            vec![teardown_group_observation(
                &launch,
                [(
                    "web",
                    if desired_state == ApplicationDeploymentDesiredState::Paused {
                        WorkloadUnitState::Stopped
                    } else {
                        WorkloadUnitState::Running
                    },
                )],
            )],
            vec![WorkloadGroupObservation::Absent {
                group_id: launch.group_id.clone(),
                correlation: launch.correlation.clone(),
                backend_id: dotcore_runtime::RuntimeBackendId(
                    "runtime/teardown-sequence-test".to_owned(),
                ),
            }],
            calls.clone(),
        );
        let controller = controller_with_runtime(desired.clone(), current.clone(), runtime);
        controller
            .remove_deployment(deployment.application_deployment_id.clone())
            .await
            .expect("remove acceptance");

        controller
            .realizer
            .reconcile()
            .await
            .expect("teardown reconciliation");

        assert_eq!(*calls.lock().expect("Runtime calls"), ["ensure-absent"]);
        assert!(
            desired
                .snapshot()
                .expect("final desired")
                .deployments
                .is_empty()
        );
        assert!(
            current
                .snapshot()
                .expect("final current")
                .deployments
                .is_empty()
        );
        std::fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn teardown_deletes_application_data_and_retries_a_failed_removal_while_pause_keeps_it() {
    for desired_state in [
        ApplicationDeploymentDesiredState::Present,
        ApplicationDeploymentDesiredState::Paused,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let mut deployment = deployment(desired_state);
        deployment.storage_attachments = vec![ApplicationStorageAttachment {
            service_name: "web".to_owned(),
            binding_name: "data".to_owned(),
            storage_name: "web-data".to_owned(),
        }];
        desired
            .upsert(deployment.clone(), 0)
            .expect("desired deployment");
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: deployment.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(queued_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("current deployment");
        let launch = workload_group_launch(
            &deployment,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("Runtime launch");
        let absent = || WorkloadGroupObservation::Absent {
            group_id: launch.group_id.clone(),
            correlation: launch.correlation.clone(),
            backend_id: dotcore_runtime::RuntimeBackendId(
                "runtime/teardown-sequence-test".to_owned(),
            ),
        };
        let calls = Arc::new(Mutex::new(Vec::new()));
        let retained = Arc::new(Mutex::new(true));
        let runtime = TeardownSequenceRuntime::with_retained_data(
            launch.clone(),
            vec![teardown_group_observation(
                &launch,
                [(
                    "web",
                    if desired_state == ApplicationDeploymentDesiredState::Paused {
                        WorkloadUnitState::Stopped
                    } else {
                        WorkloadUnitState::Running
                    },
                )],
            )],
            vec![absent(), absent()],
            Arc::clone(&calls),
            retained.clone(),
        )
        .with_data_removal_failures(1);
        let controller = controller_with_runtime(desired.clone(), current, runtime);
        let id = deployment.application_deployment_id.clone();

        controller
            .remove_deployment(id.clone())
            .await
            .expect("remove acceptance");
        controller
            .realizer
            .reconcile()
            .await
            .expect("failed removal never fails the pass");
        assert!(*retained.lock().expect("retained data"));
        let pending = controller
            .get_deployment(id.clone())
            .await
            .expect("pending teardown");
        assert_eq!(
            pending.state.as_ref().expect("state").stage,
            "runtime-cleanup",
            "a failed data removal leaves the teardown pending"
        );
        assert_eq!(desired.snapshot().expect("desired").deployments.len(), 1);

        controller
            .realizer
            .reconcile()
            .await
            .expect("retried teardown");
        assert!(
            !*retained.lock().expect("retained data"),
            "teardown deleted the data"
        );
        assert!(desired.snapshot().expect("desired").deployments.is_empty());
        assert_eq!(
            calls
                .lock()
                .expect("Runtime calls")
                .iter()
                .filter(|call| **call == "remove-data")
                .count(),
            2
        );
        std::fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn present_completes_in_process_network_only_residue_after_failed_removal() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let teardown = deployment(ApplicationDeploymentDesiredState::Teardown);
    desired
        .upsert(teardown.clone(), 0)
        .expect("teardown declaration");
    current
        .upsert(
            CurrentApplicationDeployment {
                application_deployment_id: teardown.application_deployment_id.clone(),
                record_revision: 0,
                state: Some(queued_state()),
                reconciled_lifecycle_generation: None,
            },
            None,
        )
        .expect("current deployment");
    let launch = workload_group_launch(
        &teardown,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");
    let removals = Arc::new(Mutex::new(0));
    let creations = Arc::new(Mutex::new(0));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            NetworkResidueRetryRuntime {
                launch,
                removals: Arc::clone(&removals),
                creations: Arc::clone(&creations),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        test_catalog(),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    controller
        .realizer
        .reconcile()
        .await
        .expect("failed removal reconciliation");
    assert_eq!(*removals.lock().expect("removals"), 1);
    assert_eq!(*creations.lock().expect("creations"), 0);

    let mut present = teardown;
    present.desired_state = ApplicationDeploymentDesiredState::Present as i32;
    desired
        .upsert(present, 1)
        .expect("restore PRESENT declaration");
    controller
        .realizer
        .reconcile()
        .await
        .expect("in-process network-only completion");

    assert_eq!(*removals.lock().expect("removals"), 1);
    assert_eq!(*creations.lock().expect("creations"), 1);
    assert_eq!(desired.snapshot().expect("desired").deployments.len(), 1);
    assert_eq!(current.snapshot().expect("current").deployments.len(), 1);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn display_name_never_changes_runtime_identity_or_generation() {
    let first = deployment(ApplicationDeploymentDesiredState::Present);
    let mut renamed = first.clone();
    renamed.application_deployment_name = "A duplicate-friendly display name".to_owned();

    let first_launch = workload_group_launch(
        &first,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("first runtime launch");
    let renamed_launch = workload_group_launch(
        &renamed,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("renamed runtime launch");

    assert_eq!(first_launch.group_id, renamed_launch.group_id);
    assert_eq!(first_launch.correlation, renamed_launch.correlation);
}

#[test]
fn deployment_id_is_bounded_and_retries_declared_collisions() {
    let now = UNIX_EPOCH + Duration::from_secs(1_786_543_445);
    let active = allocate_deployment_id(
        "application-with-the-longest-valid-identifier-01234567890123456789",
        now,
        std::iter::empty(),
        [Ok(0x12_ab_cd_ef_01)],
    )
    .expect("bounded deployment ID");
    let allocated = allocate_deployment_id(
        "application-with-the-longest-valid-identifier-01234567890123456789",
        now,
        [active.as_str()],
        [Ok(0x12_ab_cd_ef_01), Ok(0x12_ab_cd_ef_02)],
    )
    .expect("collision retry");

    assert_eq!(active.len(), 40);
    assert!(active.starts_with("applicat-"));
    let without_suffix = active.rsplit_once('-').expect("suffix").0;
    assert_eq!(
        without_suffix.rsplit_once('-').expect("timestamp").1.len(),
        14
    );
    assert!(active.ends_with("-12abcdef01"));
    assert!(
        allocated.ends_with("-12abcdef02"),
        "a declared identity is never allocated again"
    );
}

#[test]
fn configuration_attachments_preserve_empty_literals_and_resolve_named_values() {
    let mut application = application();
    application.graph.as_mut().expect("graph").services[0].configuration_bindings = vec![
        ApplicationConfigurationBinding {
            binding_name: "empty".to_owned(),
            environment_variable: "EMPTY_VALUE".to_owned(),
            source: Some(application_configuration_binding::Source::LiteralValue(
                String::new(),
            )),
            input: None,
        },
        ApplicationConfigurationBinding {
            binding_name: "inference".to_owned(),
            environment_variable: "INFERENCE_URL".to_owned(),
            source: Some(
                application_configuration_binding::Source::ResolvedValueName(
                    "configured-inference".to_owned(),
                ),
            ),
            input: None,
        },
    ];
    let sources = |name: &str| {
        (name == "configured-inference").then(|| "http://192.0.2.10:6470/v1".to_owned())
    };

    let attachments = configuration_attachments(&application, sources, &[], false)
        .expect("configuration attachments");

    assert_eq!(
        attachments,
        vec![
            ApplicationConfigurationAttachment {
                service_name: "web".to_owned(),
                binding_name: "empty".to_owned(),
                environment_variable: "EMPTY_VALUE".to_owned(),
                value: String::new(),
            },
            ApplicationConfigurationAttachment {
                service_name: "web".to_owned(),
                binding_name: "inference".to_owned(),
                environment_variable: "INFERENCE_URL".to_owned(),
                value: "http://192.0.2.10:6470/v1".to_owned(),
            },
        ]
    );
    application.graph.as_mut().expect("graph").services[0].configuration_bindings[1].source =
        Some(application_configuration_binding::Source::ResolvedValueName("unknown".to_owned()));
    assert!(matches!(
        configuration_attachments(&application, sources, &[], false),
        Err(ApplicationLifecycleError::Configuration(source))
            if source.to_string() == "Application configuration source is unavailable"
    ));
}

#[test]
fn resolved_urls_name_the_hosts_each_hosting_node_and_the_manager_reach() {
    let mut site = node_site(Vec::new());
    let mut split = site.nodes[1].clone();
    split.node_id = "node-two".to_owned();
    split.host = "10.0.0.3".to_owned();
    split.manager_host = "100.64.0.76".to_owned();
    site.nodes.push(split);
    let mut named = site.nodes[1].clone();
    named.node_id = "node-three".to_owned();
    named.host = "10.0.0.4".to_owned();
    named.manager_host = "manager.other.ts.net".to_owned();
    site.nodes.push(named);
    let sources = ApplicationConfigurationSources::new(
        dotcore_domain::SiteNodeHost::parse("manager.example.ts.net").expect("manager host"),
        6421,
    );
    let port = std::num::NonZeroU16::new(31_234).expect("nonzero port");
    let resolve = |name: &str, site_node_id: &str| sources.resolve(name, &site, site_node_id, port);

    assert_eq!(
        resolve("configured-inference-openai-base-url", "manager-node").as_deref(),
        Some("http://manager.example.ts.net:6421/v1")
    );
    assert_eq!(
        resolve("configured-inference-openai-base-url", "node-one").as_deref(),
        Some("http://manager.example.ts.net:6421/v1")
    );
    assert_eq!(
        resolve("configured-inference-openai-base-url", "node-two").as_deref(),
        Some("http://100.64.0.76:6421/v1")
    );
    assert_eq!(
        resolve("configured-inference-openai-base-url", "node-three").as_deref(),
        Some("http://manager.other.ts.net:6421/v1")
    );
    assert_eq!(
        resolve("configured-inference-openai-base-url", "unknown-node"),
        None
    );
    assert_eq!(
        resolve("published-endpoint-url", "manager-node").as_deref(),
        Some("http://127.0.0.1:31234")
    );
    assert_eq!(
        resolve("published-endpoint-url", "node-two").as_deref(),
        Some("http://10.0.0.3:31234")
    );
    assert_eq!(
        resolve("published-endpoint-url", "node-three").as_deref(),
        Some("http://10.0.0.4:31234")
    );
    assert_eq!(resolve("published-endpoint-url", "unknown-node"), None);
    assert_eq!(resolve("unknown-source", "node-one"), None);
}

#[tokio::test]
async fn created_deployment_retains_the_base_url_of_its_hosting_node() {
    let directory = crate::test_support::canonical_tempdir().expect("Site directory");
    let (desired, current) = open_node_site_with(directory.path(), |site| {
        site.nodes[1].manager_host = "100.64.0.76".to_owned();
    });
    let controller = ApplicationController::new(
        test_catalog(),
        desired.clone(),
        current,
        ApplicationControllerConfiguration {
            target_capabilities: node_target_capabilities(),
            configuration_sources: ApplicationConfigurationSources::new(
                dotcore_domain::SiteNodeHost::LOOPBACK,
                6421,
            ),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("manager apps controller");

    let created = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("node-hosted deployment");

    let declared = desired
        .application_snapshot()
        .expect("Site")
        .deployments
        .into_iter()
        .find(|declaration| {
            declaration.application_deployment_id == created.application_deployment_id
        })
        .expect("declaration");
    assert_eq!(
        declared
            .configuration_attachments
            .iter()
            .map(|attachment| (attachment.binding_name.as_str(), attachment.value.as_str()))
            .collect::<Vec<_>>(),
        vec![("base-url", "http://100.64.0.76:6421/v1")]
    );
}

#[tokio::test]
async fn created_deployments_hold_distinct_published_ports_through_teardown_and_restart() {
    let directory = crate::test_support::canonical_tempdir().expect("Site directory");
    let (desired, current) = open_node_site_with(directory.path(), |_| {});
    let mut webui = application();
    webui.graph.as_mut().expect("graph").services[0]
        .configuration_bindings
        .push(ApplicationConfigurationBinding {
            binding_name: "public-url".to_owned(),
            environment_variable: "PUBLIC_URL".to_owned(),
            source: Some(
                application_configuration_binding::Source::ResolvedValueName(
                    "published-endpoint-url".to_owned(),
                ),
            ),
            input: None,
        });
    let controller = ApplicationController::new(
        Arc::new(
            CatalogBuilder::new()
                .oci_artifact_policy(OciArtifactPolicy::Development)
                .engine_validator(crate::model::engine::declaration_validator())
                .application("open-webui", webui)
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current,
        ApplicationControllerConfiguration {
            target_capabilities: node_target_capabilities(),
            configuration_sources: ApplicationConfigurationSources::new(
                dotcore_domain::SiteNodeHost::LOOPBACK,
                6421,
            ),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("manager apps controller");
    let request = || CreateApplicationDeploymentRequest {
        application_id: "open-webui".to_owned(),
        placement: Some(exact_placement()),
        inference_route_id: None,
        expected_inference_route_generation: 0,
        application_deployment_name: None,
        parameter_values: Vec::new(),
    };
    let declared = |desired: &SiteRepository, deployment_id: &str| {
        desired
            .application_snapshot()
            .expect("Site")
            .deployments
            .into_iter()
            .find(|declaration| declaration.application_deployment_id == deployment_id)
            .expect("declaration")
    };

    let (first, second) = tokio::join!(
        controller.create_deployment(request()),
        controller.create_deployment(request())
    );
    let first = declared(&desired, &first.expect("first").application_deployment_id);
    let second = declared(&desired, &second.expect("second").application_deployment_id);
    assert_ne!(first.published_port, second.published_port);
    for declaration in [&first, &second] {
        assert!((30_000..=32_767).contains(&declaration.published_port));
        assert_eq!(
            declaration
                .configuration_attachments
                .iter()
                .map(|attachment| (attachment.binding_name.as_str(), attachment.value.clone()))
                .collect::<Vec<_>>(),
            vec![(
                "public-url",
                format!("http://{WORKER_HOST}:{}", declaration.published_port)
            )]
        );
    }

    controller
        .remove_deployment(first.application_deployment_id.clone())
        .await
        .expect("teardown accepted");
    let third = controller
        .create_deployment(request())
        .await
        .expect("third");
    let third = declared(&desired, &third.application_deployment_id);
    assert!(
        ![first.published_port, second.published_port].contains(&third.published_port),
        "a deployment still tearing down keeps its port"
    );

    let reloaded = SiteRepository::load(
        &directory.path().join("site.yaml"),
        crate::test_support::site_declaration_rules(),
    )
    .expect("reloaded site");
    let retained = declared(&reloaded, &second.application_deployment_id);
    assert_eq!(retained.published_port, second.published_port);
    let launch = workload_group_launch(
        &retained,
        Ipv4Addr::new(10, 0, 0, 2),
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    assert_eq!(
        u32::from(launch.publication.host_port.get()),
        second.published_port
    );
}

#[tokio::test]
async fn created_deployment_snapshot_retains_the_application_notes() {
    let directory = crate::test_support::canonical_tempdir().expect("Site directory");
    let (desired, current) = open_node_site_with(directory.path(), |_| {});
    let mut webui = application();
    webui.notes = "Sign in with the fixed login.\nPick the credential.".to_owned();
    let controller = ApplicationController::new(
        Arc::new(
            CatalogBuilder::new()
                .oci_artifact_policy(OciArtifactPolicy::Development)
                .engine_validator(crate::model::engine::declaration_validator())
                .application("open-webui", webui)
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current,
        ApplicationControllerConfiguration {
            target_capabilities: node_target_capabilities(),
            configuration_sources: ApplicationConfigurationSources::new(
                dotcore_domain::SiteNodeHost::LOOPBACK,
                6421,
            ),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("manager apps controller");

    let created = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("deployment");

    let declared = desired
        .application_snapshot()
        .expect("Site")
        .deployments
        .into_iter()
        .find(|declaration| {
            declaration.application_deployment_id == created.application_deployment_id
        })
        .expect("declaration");
    assert_eq!(
        declared.snapshot.expect("snapshot").notes,
        "Sign in with the fixed login.\nPick the credential."
    );
}

#[test]
fn launch_pins_only_site_host_names_from_site_host_sources() {
    let launch_with_base_url = |base_url: &str| {
        let mut deployment = deployment(ApplicationDeploymentDesiredState::Present);
        let service = &mut deployment
            .snapshot
            .as_mut()
            .expect("snapshot")
            .graph
            .as_mut()
            .expect("graph")
            .services[0];
        let service_name = service.service_name.clone();
        for (binding_name, environment_variable, source) in [
            (
                "base-url",
                "OPENAI_API_BASE_URL",
                application_configuration_binding::Source::ResolvedValueName(
                    "configured-inference-openai-base-url".to_owned(),
                ),
            ),
            (
                "upstream-url",
                "UPSTREAM_BASE_URL",
                application_configuration_binding::Source::LiteralValue(
                    "https://api.openai.com/v1".to_owned(),
                ),
            ),
        ] {
            service
                .configuration_bindings
                .push(ApplicationConfigurationBinding {
                    binding_name: binding_name.to_owned(),
                    environment_variable: environment_variable.to_owned(),
                    source: Some(source),
                    input: None,
                });
        }
        deployment.configuration_attachments = vec![
            ApplicationConfigurationAttachment {
                service_name: service_name.clone(),
                binding_name: "base-url".to_owned(),
                environment_variable: "OPENAI_API_BASE_URL".to_owned(),
                value: base_url.to_owned(),
            },
            ApplicationConfigurationAttachment {
                service_name,
                binding_name: "upstream-url".to_owned(),
                environment_variable: "UPSTREAM_BASE_URL".to_owned(),
                value: "https://api.openai.com/v1".to_owned(),
            },
        ];
        workload_group_launch(
            &deployment,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("launch")
    };

    assert_eq!(
        launch_with_base_url("http://manager.example.ts.net:6421/v1").host_aliases,
        BTreeSet::from([
            dotcore_domain::SiteNodeHost::parse("manager.example.ts.net").expect("host")
        ])
    );
    assert_eq!(
        launch_with_base_url("http://192.0.2.10:6421/v1").host_aliases,
        BTreeSet::new()
    );
}

#[test]
fn configuration_attachments_reject_secret_bindings() {
    let mut application = application();
    application.graph.as_mut().expect("graph").services[0]
        .secret_bindings
        .push(ApplicationSecretBinding {
            binding_name: "secret".to_owned(),
            environment_variable: "SECRET_VALUE".to_owned(),
        });

    assert!(matches!(
        configuration_attachments(&application, |_| None, &[], false),
        Err(ApplicationLifecycleError::FailedPrecondition)
    ));
}

#[test]
fn route_name_binding_retains_its_literal_only_without_a_route_link() {
    let mut application = application_with_route_input();
    application.graph.as_mut().expect("graph").services[0]
        .configuration_bindings
        .push(ApplicationConfigurationBinding {
            binding_name: "base-url".to_owned(),
            environment_variable: "INFERENCE_BASE_URL".to_owned(),
            source: Some(
                application_configuration_binding::Source::ResolvedValueName(
                    "configured-inference".to_owned(),
                ),
            ),
            input: None,
        });
    let sources = |name: &str| {
        (name == "configured-inference").then(|| "http://192.0.2.10:6470/v1".to_owned())
    };
    let base_url = ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "base-url".to_owned(),
        environment_variable: "INFERENCE_BASE_URL".to_owned(),
        value: "http://192.0.2.10:6470/v1".to_owned(),
    };

    assert_eq!(
        configuration_attachments(&application, sources, &[], true).expect("linked route"),
        vec![base_url.clone()]
    );
    assert_eq!(
        configuration_attachments(&application, sources, &[], false).expect("no route link"),
        vec![
            ApplicationConfigurationAttachment {
                service_name: "web".to_owned(),
                binding_name: "route".to_owned(),
                environment_variable: "ROUTE_NAME".to_owned(),
                value: "default-route".to_owned(),
            },
            base_url,
        ]
    );
}

#[test]
fn workload_launch_uses_persisted_configuration_attachments() {
    let mut deployment = deployment(ApplicationDeploymentDesiredState::Present);
    let service = &mut deployment
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0];
    service.configuration_bindings = vec![ApplicationConfigurationBinding {
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        source: Some(
            application_configuration_binding::Source::ResolvedValueName(
                "configured-api".to_owned(),
            ),
        ),
        input: None,
    }];
    deployment.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        value: "http://192.0.2.10:6470/v1".to_owned(),
    }];

    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("workload launch");

    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([("API_URL".to_owned(), "http://192.0.2.10:6470/v1".to_owned(),)])
    );
}

#[test]
fn changed_configuration_attachments_reject_stale_workload_observations() {
    let mut persisted = deployment(ApplicationDeploymentDesiredState::Present);
    let service = &mut persisted
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0];
    service.configuration_bindings = vec![ApplicationConfigurationBinding {
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        source: Some(
            application_configuration_binding::Source::ResolvedValueName(
                "configured-api".to_owned(),
            ),
        ),
        input: None,
    }];
    persisted.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        value: "http://192.0.2.10:6470/v1".to_owned(),
    }];
    let stale_launch = workload_group_launch(
        &persisted,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("stale workload launch");
    let mut changed = persisted.clone();
    changed.configuration_attachments[0].value = "http://192.0.2.11:6470/v1".to_owned();
    let expected_launch = workload_group_launch(
        &changed,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("expected workload launch");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtime_builder = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtime_builder
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let runtimes = runtime_builder.build();
    let stale_observation = TargetWorkloadGroupObservation {
        execution_target_id: "application-target-one".to_owned(),
        observation: WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
            group_id: stale_launch.group_id.clone(),
            correlation: stale_launch.correlation.clone(),
            backend_id: runtimes
                .backend_id("application-target-one")
                .expect("Runtime backend"),
            units: stale_launch
                .units
                .keys()
                .cloned()
                .map(|name| {
                    (
                        name,
                        dotcore_runtime::WorkloadUnitObservation {
                            state: WorkloadUnitState::Running,
                            ready_launch_gate: None,
                            host_process: None,
                        },
                    )
                })
                .collect(),
            assigned_endpoint: None,
        }),
    };

    assert_ne!(
        stale_launch.correlation.reconcile_generation,
        expected_launch.correlation.reconcile_generation
    );
    assert!(
        exact_owned_observation(
            &stale_observation,
            &stale_launch,
            "application-target-one",
            &runtimes
        )
        .is_some()
    );
    assert!(
        exact_owned_observation(
            &stale_observation,
            &expected_launch,
            "application-target-one",
            &runtimes
        )
        .is_none()
    );

    let other_node_observation = TargetWorkloadGroupObservation {
        execution_target_id: "application-target-two".to_owned(),
        observation: stale_observation.observation.clone(),
    };
    assert!(
        exact_owned_observation(
            &other_node_observation,
            &stale_launch,
            "application-target-one",
            &runtimes
        )
        .is_none()
    );
    assert!(
        stale_owned_group(
            &other_node_observation,
            &stale_launch,
            "application-target-one",
            &runtimes
        )
        .is_none()
    );
}

#[tokio::test]
async fn deployment_creation_persists_the_route_link_without_a_route_name_attachment() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_route_input())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    let deployment = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: Some("route-id".to_owned()),
            expected_inference_route_generation: 7,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("deployment creation");

    let desired = desired.snapshot().expect("persisted desired deployment");
    assert_eq!(desired.deployments.len(), 1);
    assert_eq!(desired.deployments[0].lifecycle_generation, 1);
    assert!(desired.deployments[0].configuration_attachments.is_empty());
    assert_eq!(
        desired.deployments[0].inference_route_id.as_deref(),
        Some("route-id")
    );
    assert_eq!(
        deployment.selected_inference_route,
        Some(ApplicationSelectedInferenceRoute {
            inference_route_id: "route-id".to_owned(),
            route_name: "route-one".to_owned(),
        })
    );
    assert_eq!(
        current
            .snapshot()
            .expect("current deployment")
            .deployments
            .len(),
        1
    );
    assert!(deployment.snapshot.is_some());
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn route_input_uses_its_literal_without_a_route_link() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_route_input())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver_with(Err(
                InferenceRouteResolutionError::Stale,
            )),
        },
    )
    .expect("Apps controller");

    controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("optional route default");

    let mut snapshot = desired.snapshot().expect("persisted declaration");
    let declaration = snapshot.deployments.remove(0);
    assert_eq!(declaration.inference_route_id, None);
    assert_eq!(
        declaration.configuration_attachments[0].value,
        "default-route"
    );
    let launch = workload_group_launch(
        &declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch from optional default");
    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].environment["ROUTE_NAME"],
        "default-route"
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn invalid_route_binding_identity_or_generation_rejects_without_mutation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_route_input())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    for (inference_route_id, expected_inference_route_generation) in [
        (Some("invalid route".to_owned()), 7),
        (Some("route-id".to_owned()), 0),
        (None, 7),
    ] {
        let result = controller
            .create_deployment(CreateApplicationDeploymentRequest {
                application_id: "open-webui".to_owned(),
                placement: Some(exact_placement()),
                inference_route_id,
                expected_inference_route_generation,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            })
            .await;
        assert!(matches!(
            result,
            Err(ApplicationLifecycleError::InvalidArgument)
        ));
    }
    assert!(
        desired
            .snapshot()
            .expect("desired state")
            .deployments
            .is_empty()
    );
    assert!(
        current
            .snapshot()
            .expect("current state")
            .deployments
            .is_empty()
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn route_binding_without_projection_rejects_before_resolution_or_mutation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let route_resolution_calls = Arc::new(AtomicUsize::new(0));
    let runtime_calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: runtime_calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("open-webui", application())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: Arc::new(CountingCreateInferenceRouteResolver {
                calls: route_resolution_calls.clone(),
            }),
        },
    )
    .expect("Apps controller");

    let result = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: Some("route-id".to_owned()),
            expected_inference_route_generation: 7,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await;

    assert!(matches!(
        result,
        Err(ApplicationLifecycleError::InvalidArgument)
    ));
    assert_eq!(route_resolution_calls.load(Ordering::Relaxed), 0);
    assert_eq!(runtime_calls.lock().expect("Runtime calls").len(), 0);
    assert_eq!(
        desired.snapshot().expect("desired state").deployments,
        Vec::new()
    );
    assert_eq!(
        current.snapshot().expect("current state").deployments,
        Vec::new()
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn route_binding_failures_reject_before_site_store_or_runtime_mutation() {
    for resolution in [
        InferenceRouteResolutionError::Unknown,
        InferenceRouteResolutionError::Stale,
        InferenceRouteResolutionError::Unavailable,
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
        runtimes
            .insert(
                "application-target-one",
                RestartRuntime {
                    calls: calls.clone(),
                },
            )
            .expect("Runtime adapter");
        let controller = TestController::new(
            Arc::new(
                CatalogBuilder::new()
                    .application("test", application_with_route_input())
                    .build()
                    .expect("Catalog"),
            ),
            desired.clone(),
            current.clone(),
            runtimes.build(),
            ApplicationControllerConfiguration {
                target_capabilities: test_target_capability_source(),
                configuration_sources: ApplicationConfigurationSources::default(),
                inference_route_resolver: test_inference_route_resolver_with(Err(resolution)),
            },
        )
        .expect("Apps controller");

        let result = controller
            .create_deployment(CreateApplicationDeploymentRequest {
                application_id: "open-webui".to_owned(),
                placement: Some(exact_placement()),
                inference_route_id: Some("route-id".to_owned()),
                expected_inference_route_generation: 7,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            })
            .await;

        assert!(matches!(
            result,
            Err(ApplicationLifecycleError::InferenceRoute(error)) if error == resolution
        ));
        assert!(
            desired
                .snapshot()
                .expect("desired state")
                .deployments
                .is_empty()
        );
        assert!(
            current
                .snapshot()
                .expect("current state")
                .deployments
                .is_empty()
        );
        assert!(calls.lock().expect("Runtime calls").is_empty());
        drop(controller);
        std::fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[test]
fn launch_resolves_all_route_projections_once_from_one_current_result() {
    let mut selected = deployment(ApplicationDeploymentDesiredState::Present);
    let graph = selected
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph");
    graph.services[0].configuration_bindings = vec![
        route_configuration_binding(
            "route",
            "ROUTE_NAME",
            ApplicationInferenceRouteProjection::RouteName,
        ),
        ApplicationConfigurationBinding {
            binding_name: "fixed".to_owned(),
            environment_variable: "FIXED_VALUE".to_owned(),
            ..Default::default()
        },
    ];
    let mut node = graph.services[0].clone();
    node.service_name = "node".to_owned();
    node.configuration_bindings = vec![route_configuration_binding(
        "model",
        "MODEL",
        ApplicationInferenceRouteProjection::RouteName,
    )];
    graph.services.push(node);
    synchronize_service_selections(&mut selected);
    selected.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "fixed".to_owned(),
        environment_variable: "FIXED_VALUE".to_owned(),
        value: "fixed-value".to_owned(),
    }];
    selected.inference_route_id = Some("route-id".to_owned());
    let calls = Arc::new(AtomicUsize::new(0));

    let launch = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        &ChangingCurrentInferenceRouteResolver {
            calls: Arc::clone(&calls),
        },
    )
    .expect("workload launch");

    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([
            ("FIXED_VALUE".to_owned(), "fixed-value".to_owned()),
            ("ROUTE_NAME".to_owned(), "route-1".to_owned()),
        ])
    );
    assert_eq!(
        launch.units[&WorkloadUnitName("node".to_owned())].environment,
        BTreeMap::from([("MODEL".to_owned(), "route-1".to_owned())])
    );
    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].reconfigurable_environment,
        BTreeSet::from(["ROUTE_NAME".to_owned()])
    );
    assert_eq!(
        launch.units[&WorkloadUnitName("node".to_owned())].reconfigurable_environment,
        BTreeSet::from(["MODEL".to_owned()])
    );

    let mut ordinary_only = selected.clone();
    ordinary_only
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services
        .truncate(1);
    ordinary_only
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0]
        .configuration_bindings = vec![ApplicationConfigurationBinding {
        binding_name: "fixed".to_owned(),
        environment_variable: "FIXED_VALUE".to_owned(),
        ..Default::default()
    }];
    synchronize_service_selections(&mut ordinary_only);
    let ordinary_calls = Arc::new(AtomicUsize::new(0));
    let ordinary_launch = workload_group_launch(
        &ordinary_only,
        Ipv4Addr::LOCALHOST,
        &ChangingCurrentInferenceRouteResolver {
            calls: Arc::clone(&ordinary_calls),
        },
    )
    .expect("ordinary launch");
    assert_eq!(ordinary_calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        ordinary_launch.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([("FIXED_VALUE".to_owned(), "fixed-value".to_owned())])
    );
    assert!(
        ordinary_launch.units[&WorkloadUnitName("web".to_owned())]
            .reconfigurable_environment
            .is_empty()
    );

    let mut optional_unselected = ordinary_only;
    optional_unselected.inference_route_id = None;
    optional_unselected
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0]
        .configuration_bindings = vec![route_configuration_binding(
        "route",
        "ROUTE_NAME",
        ApplicationInferenceRouteProjection::RouteName,
    )];
    optional_unselected.configuration_attachments[0].binding_name = "route".to_owned();
    optional_unselected.configuration_attachments[0].environment_variable = "ROUTE_NAME".to_owned();
    optional_unselected.configuration_attachments[0].value = "attachment-fallback".to_owned();
    let optional_calls = Arc::new(AtomicUsize::new(0));
    let optional_launch = workload_group_launch(
        &optional_unselected,
        Ipv4Addr::LOCALHOST,
        &ChangingCurrentInferenceRouteResolver {
            calls: Arc::clone(&optional_calls),
        },
    )
    .expect("optional launch");
    assert_eq!(optional_calls.load(Ordering::Relaxed), 0);
    assert_eq!(
        optional_launch.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([("ROUTE_NAME".to_owned(), "attachment-fallback".to_owned())])
    );
    assert!(
        optional_launch.units[&WorkloadUnitName("web".to_owned())]
            .reconfigurable_environment
            .is_empty()
    );
}

#[test]
fn route_link_launches_with_current_name_and_retained_base_url() {
    let mut selected = deployment(ApplicationDeploymentDesiredState::Present);
    let service = &mut selected
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0];
    service.configuration_bindings = vec![
        route_configuration_binding(
            "route",
            "ROUTE_NAME",
            ApplicationInferenceRouteProjection::RouteName,
        ),
        ApplicationConfigurationBinding {
            binding_name: "inference".to_owned(),
            environment_variable: "INFERENCE_URL".to_owned(),
            source: Some(
                application_configuration_binding::Source::ResolvedValueName(
                    "configured-inference".to_owned(),
                ),
            ),
            input: None,
        },
    ];
    selected.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "inference".to_owned(),
        environment_variable: "INFERENCE_URL".to_owned(),
        value: "http://192.0.2.10:6470/v1".to_owned(),
    }];
    selected.inference_route_id = Some("route-id".to_owned());

    let first = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver_for("route-one").as_ref(),
    )
    .expect("first workload launch");
    let renamed = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver_for("route-renamed").as_ref(),
    )
    .expect("renamed-route workload launch");

    assert_eq!(
        first.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([
            (
                "INFERENCE_URL".to_owned(),
                "http://192.0.2.10:6470/v1".to_owned()
            ),
            ("ROUTE_NAME".to_owned(), "route-one".to_owned()),
        ])
    );
    assert_eq!(
        renamed.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([
            (
                "INFERENCE_URL".to_owned(),
                "http://192.0.2.10:6470/v1".to_owned()
            ),
            ("ROUTE_NAME".to_owned(), "route-renamed".to_owned()),
        ])
    );
    assert_eq!(
        renamed.units[&WorkloadUnitName("web".to_owned())].reconfigurable_environment,
        BTreeSet::from(["ROUTE_NAME".to_owned()])
    );
    assert_ne!(
        first.correlation.reconcile_generation,
        renamed.correlation.reconcile_generation
    );
}

#[test]
fn policy_only_route_changes_do_not_recreate_when_launch_projection_is_unchanged() {
    let mut selected = deployment(ApplicationDeploymentDesiredState::Present);
    selected.snapshot = Some(resolved_snapshot(application_with_route_input()));
    selected.inference_route_id = Some("route-id".to_owned());

    let generation_before = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver_for("route-one").as_ref(),
    )
    .expect("launch before policy edit")
    .correlation
    .reconcile_generation;
    let generation_after = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver_for("route-one").as_ref(),
    )
    .expect("launch after policy edit")
    .correlation
    .reconcile_generation;

    assert_eq!(generation_after, generation_before);
}

#[tokio::test]
async fn malformed_persisted_launch_declarations_fail_before_resolution_or_runtime_mutation() {
    let cases: [(&str, DeploymentMutator); 9] = [
        ("missing graph", |declaration| {
            declaration.snapshot.as_mut().expect("snapshot").graph = None
        }),
        ("missing primary endpoint", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .primary_endpoint = None;
        }),
        ("duplicated attachment", |declaration| {
            declaration.storage_attachments = vec![
                ApplicationStorageAttachment {
                    service_name: "web".to_owned(),
                    binding_name: "data".to_owned(),
                    storage_name: "data".to_owned(),
                },
                ApplicationStorageAttachment {
                    service_name: "web".to_owned(),
                    binding_name: "data".to_owned(),
                    storage_name: "data-two".to_owned(),
                },
            ];
        }),
        ("missing configuration attachment", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .configuration_bindings
                .push(ApplicationConfigurationBinding {
                    binding_name: "missing".to_owned(),
                    environment_variable: "MISSING".to_owned(),
                    ..Default::default()
                });
        }),
        ("missing storage attachment", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .storage_bindings
                .push(StorageBinding {
                    binding_name: "missing".to_owned(),
                    mount_path: "/missing".to_owned(),
                    kind: StorageKind::Data as i32,
                    scope: StorageScope::Deployment as i32,
                    environment: Vec::new(),
                });
        }),
        ("missing artifact", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .artifact = None;
        }),
        ("invalid dependency", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .dependencies
                .push(ApplicationServiceDependency {
                    service_name: "node".to_owned(),
                    condition: 0,
                });
        }),
        ("invalid readiness", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .readiness = Some(ApplicationServiceReadiness {
                check: Some(application_service_readiness::Check::HttpGet(
                    ApplicationHttpReadiness {
                        port_name: String::new(),
                        path: "/health".to_owned(),
                    },
                )),
            });
        }),
        ("invalid route projection", |declaration| {
            declaration
                .snapshot
                .as_mut()
                .expect("snapshot")
                .graph
                .as_mut()
                .expect("graph")
                .services[0]
                .configuration_bindings = vec![route_configuration_binding(
                "route",
                "ROUTE_NAME",
                ApplicationInferenceRouteProjection::Unspecified,
            )];
        }),
    ];

    for (name, mutate) in cases {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
        declaration.inference_route_id = Some("route-id".to_owned());
        mutate(&mut declaration);
        desired.seed(declaration);
        let resolver_calls = Arc::new(AtomicUsize::new(0));
        let runtime_calls = Arc::new(Mutex::new(Vec::new()));
        let controller = controller_with_runtime_and_resolver(
            desired,
            current.clone(),
            RestartRuntime {
                calls: Arc::clone(&runtime_calls),
            },
            Arc::new(ChangingCurrentInferenceRouteResolver {
                calls: Arc::clone(&resolver_calls),
            }),
        );

        controller
            .realizer
            .reconcile()
            .await
            .unwrap_or_else(|error| panic!("{name}: reconciliation failed: {error:?}"));

        assert_eq!(resolver_calls.load(Ordering::Relaxed), 0, "{name}");
        assert!(
            runtime_calls.lock().expect("Runtime calls").is_empty(),
            "{name}"
        );
        assert_eq!(
            current.snapshot().expect("current state").deployments[0]
                .state
                .as_ref()
                .expect("failed deployment state")
                .stage,
            "launch-preparation",
            "{name}"
        );
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn reconciliation_relaunches_only_when_the_linked_route_name_changes() {
    let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
    declaration.snapshot = Some(resolved_snapshot(application_with_route_input()));
    declaration.inference_route_id = Some("route-id".to_owned());
    let original_launch = workload_group_launch(
        &declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("original launch");

    for (route_name, expected_calls) in [
        ("route-one", Vec::new()),
        ("route-renamed", vec!["remove", "create"]),
    ] {
        let directory = test_directory();
        let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
            .expect("declaration repository");
        let current = TestCurrentApplicationRepository::open(&directory, "site-one")
            .expect("current repository");
        desired
            .upsert(declaration.clone(), 0)
            .expect("desired declaration");
        let retained_before = desired.snapshot().expect("retained deployment intent");
        let calls = Arc::new(Mutex::new(Vec::new()));
        let removed_groups = Arc::new(Mutex::new(Vec::new()));
        let current_launch = workload_group_launch(
            &declaration,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver_for(route_name).as_ref(),
        )
        .expect("current launch");
        assert_eq!(
            current_launch.correlation == original_launch.correlation,
            expected_calls.is_empty(),
            "{route_name}"
        );
        let controller = controller_with_runtime_and_resolver(
            desired,
            current.clone(),
            PausedMatrixRuntime::with_recovery(
                vec![group_observation(
                    &original_launch,
                    [("web", WorkloadUnitState::Running)],
                    false,
                )],
                group_observation(
                    &current_launch,
                    [("web", WorkloadUnitState::Running)],
                    false,
                ),
                Arc::clone(&calls),
                Arc::clone(&removed_groups),
            ),
            test_inference_route_resolver_for(route_name),
        );

        let observed = TargetWorkloadGroupObservation {
            execution_target_id: "application-target-one".to_owned(),
            observation: group_observation(
                &original_launch,
                [("web", WorkloadUnitState::Running)],
                false,
            ),
        };
        assert_eq!(
            stale_owned_group(
                &observed,
                &current_launch,
                "application-target-one",
                &controller.realizer.runtimes,
            )
            .is_some(),
            !expected_calls.is_empty(),
            "{route_name}"
        );
        controller
            .realizer
            .reconcile()
            .await
            .expect("reconciliation");

        assert_eq!(
            controller
                .desired
                .snapshot()
                .expect("retained deployment intent after reconciliation"),
            retained_before,
            "listener reprojection must not mutate retained deployment intent"
        );
        assert_eq!(
            current_launch.units[&WorkloadUnitName("web".to_owned())].environment["ROUTE_NAME"],
            route_name
        );
        assert_eq!(current_launch.group_id, original_launch.group_id);
        assert_eq!(
            current_launch.execution_target,
            original_launch.execution_target
        );
        assert_eq!(current_launch.publication, original_launch.publication);
        assert_eq!(
            *calls.lock().expect("Runtime calls"),
            expected_calls,
            "{route_name}, state: {:#?}",
            current.snapshot().expect("current state")
        );
        assert_eq!(
            *removed_groups.lock().expect("Removed workload groups"),
            if expected_calls.is_empty() {
                Vec::new()
            } else {
                vec![observed_group(
                    &original_launch,
                    [("web", WorkloadUnitState::Running)],
                    dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned()),
                )]
            },
            "{route_name} removes the old observed group before recreation"
        );
        drop(controller);
        fs::remove_dir_all(directory).expect("test directory cleanup");
    }
}

#[tokio::test]
async fn reconciliation_replaces_changed_completed_non_route_projection() {
    let mut original_declaration = deployment(ApplicationDeploymentDesiredState::Present);
    let service = &mut original_declaration
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services[0];
    service.configuration_bindings = vec![ApplicationConfigurationBinding {
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        source: Some(
            application_configuration_binding::Source::ResolvedValueName(
                "configured-api".to_owned(),
            ),
        ),
        input: None,
    }];
    original_declaration.configuration_attachments = vec![ApplicationConfigurationAttachment {
        service_name: "web".to_owned(),
        binding_name: "api".to_owned(),
        environment_variable: "API_URL".to_owned(),
        value: "http://192.0.2.10:6470/v1".to_owned(),
    }];
    let original_launch = workload_group_launch(
        &original_declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("original launch");
    let mut changed_declaration = original_declaration.clone();
    changed_declaration.configuration_attachments[0].value = "http://192.0.2.11:6470/v1".to_owned();
    let changed_launch = workload_group_launch(
        &changed_declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("changed launch");
    assert!(changed_declaration.inference_route_id.is_none());
    assert_ne!(
        original_launch.correlation.reconcile_generation,
        changed_launch.correlation.reconcile_generation
    );

    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    desired
        .upsert(changed_declaration, 0)
        .expect("changed declaration");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let removed_groups = Arc::new(Mutex::new(Vec::new()));
    let controller = controller_with_runtime(
        desired,
        current,
        PausedMatrixRuntime::with_recovery(
            vec![group_observation(
                &original_launch,
                [("web", WorkloadUnitState::Running)],
                false,
            )],
            group_observation(
                &changed_launch,
                [("web", WorkloadUnitState::Running)],
                false,
            ),
            Arc::clone(&calls),
            Arc::clone(&removed_groups),
        ),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert_eq!(*calls.lock().expect("Runtime calls"), ["remove", "create"]);
    assert_eq!(
        *removed_groups.lock().expect("Removed workload groups"),
        vec![observed_group(
            &original_launch,
            [("web", WorkloadUnitState::Running)],
            dotcore_runtime::RuntimeBackendId("runtime/paused-matrix-test".to_owned()),
        )]
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn unresolvable_linked_route_fails_launch_without_substituting_another_route() {
    let mut selected = deployment(ApplicationDeploymentDesiredState::Present);
    selected.snapshot = Some(resolved_snapshot(application_with_route_input()));
    selected.inference_route_id = Some("route-id".to_owned());

    for category in [
        InferenceRouteResolutionError::Unknown,
        InferenceRouteResolutionError::Unavailable,
    ] {
        assert!(matches!(
            workload_group_launch(
                &selected,
                Ipv4Addr::LOCALHOST,
                test_inference_route_resolver_with(Err(category)).as_ref(),
            ),
            Err(ApplicationLifecycleError::InferenceRoute(error)) if error == category
        ));
    }
}

#[tokio::test]
async fn unknown_linked_route_is_a_typed_launch_failure_without_runtime_mutation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
    declaration.snapshot = Some(resolved_snapshot(application_with_route_input()));
    declaration.inference_route_id = Some("route-id".to_owned());
    desired.seed(declaration);
    let runtime_calls = Arc::new(Mutex::new(Vec::new()));
    let controller = controller_with_runtime_and_resolver(
        desired,
        current.clone(),
        RestartRuntime {
            calls: Arc::clone(&runtime_calls),
        },
        test_inference_route_resolver_with(Err(InferenceRouteResolutionError::Unknown)),
    );

    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");

    assert!(runtime_calls.lock().expect("Runtime calls").is_empty());
    let state = current.snapshot().expect("current state").deployments[0]
        .state
        .clone()
        .expect("failed deployment state");
    assert_eq!(
        (state.lifecycle_state, state.stage.as_str()),
        (
            ApplicationLifecycleState::Unavailable as i32,
            "launch-preparation"
        )
    );
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn canonical_generation_ignores_valid_ordering_and_changes_for_every_runtime_projection() {
    let mut baseline = deployment(ApplicationDeploymentDesiredState::Present);
    let graph = baseline
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph");
    let web = &mut graph.services[0];
    web.arguments = vec!["--serve".to_owned()];
    web.configuration_bindings = vec![
        ApplicationConfigurationBinding {
            binding_name: "config".to_owned(),
            environment_variable: "CONFIG".to_owned(),
            ..Default::default()
        },
        ApplicationConfigurationBinding {
            binding_name: "config-two".to_owned(),
            environment_variable: "CONFIG_TWO".to_owned(),
            ..Default::default()
        },
    ];
    web.storage_bindings = vec![
        StorageBinding {
            binding_name: "data".to_owned(),
            mount_path: "/data".to_owned(),
            kind: StorageKind::Data as i32,
            scope: StorageScope::Deployment as i32,
            environment: Vec::new(),
        },
        StorageBinding {
            binding_name: "cache".to_owned(),
            mount_path: "/cache".to_owned(),
            kind: StorageKind::Data as i32,
            scope: StorageScope::Deployment as i32,
            environment: Vec::new(),
        },
    ];
    web.ports.push(ApplicationServicePort {
        port_name: "http-alt".to_owned(),
        port_number: 8081,
    });
    web.readiness = Some(ApplicationServiceReadiness {
        check: Some(application_service_readiness::Check::HttpGet(
            ApplicationHttpReadiness {
                port_name: "http".to_owned(),
                path: "/health".to_owned(),
            },
        )),
    });
    let mut node = web.clone();
    node.service_name = "node".to_owned();
    node.configuration_bindings.clear();
    node.storage_bindings.clear();
    node.dependencies.clear();
    graph.services.push(node);
    graph.services[0].dependencies = vec![ApplicationServiceDependency {
        service_name: "node".to_owned(),
        condition: ApplicationDependencyCondition::Ready as i32,
    }];
    synchronize_service_selections(&mut baseline);
    baseline.storage_attachments = vec![
        ApplicationStorageAttachment {
            service_name: "web".to_owned(),
            binding_name: "data".to_owned(),
            storage_name: "data".to_owned(),
        },
        ApplicationStorageAttachment {
            service_name: "web".to_owned(),
            binding_name: "cache".to_owned(),
            storage_name: "cache".to_owned(),
        },
    ];
    baseline.configuration_attachments = vec![
        ApplicationConfigurationAttachment {
            service_name: "web".to_owned(),
            binding_name: "config".to_owned(),
            environment_variable: "CONFIG".to_owned(),
            value: "config-one".to_owned(),
        },
        ApplicationConfigurationAttachment {
            service_name: "web".to_owned(),
            binding_name: "config-two".to_owned(),
            environment_variable: "CONFIG_TWO".to_owned(),
            value: "config-two".to_owned(),
        },
    ];
    let generation = |declaration: &ApplicationDeploymentDeclaration, host| {
        workload_group_launch(declaration, host, test_inference_route_resolver().as_ref())
            .expect("valid runtime launch")
            .correlation
            .reconcile_generation
    };
    let baseline_generation = generation(&baseline, Ipv4Addr::LOCALHOST);

    let mut reordered = baseline.clone();
    reordered.storage_attachments.reverse();
    reordered.configuration_attachments.reverse();
    reordered
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph")
        .services
        .reverse();
    assert_eq!(
        generation(&reordered, Ipv4Addr::LOCALHOST),
        baseline_generation
    );

    let changes: [LaunchProjectionChange; 18] = [
        (
            "execution target",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration.placement = Some(ExactPlacement {
                    site_node_id: "node-two".to_owned(),
                    execution_target_id: "application-target-two".to_owned(),
                })
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "image",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .artifact = Some(digest_artifact(
                    Some("example.test"),
                    "changed-image",
                    "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                    None,
                ));
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "command",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .command = vec!["changed-command".to_owned()]
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "arguments",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .arguments = vec!["--changed".to_owned()]
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "environment",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration.configuration_attachments[0].value = "config-two".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "environment key",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .configuration_bindings[0]
                    .environment_variable = "UPDATED_CONFIG".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "storage",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration.storage_attachments[0].storage_name = "data-two".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "storage destination",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .storage_bindings[0]
                    .mount_path = "/updated-data".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "ports",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .ports[0]
                    .port_number = 9090
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "port name",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .ports[1]
                    .port_name = "updated-http-alt".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "unit name",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                let graph = declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph");
                graph.services[1].service_name = "updated-worker".to_owned();
                graph.services[0].dependencies[0].service_name = "updated-worker".to_owned();
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "dependencies",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .dependencies[0]
                    .condition = ApplicationDependencyCondition::Started as i32
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "dependency unit name",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .dependencies[0]
                    .service_name = "other-worker".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "readiness",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                let application_service_readiness::Check::HttpGet(http) = declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[1]
                    .readiness
                    .as_mut()
                    .expect("readiness")
                    .check
                    .as_mut()
                    .expect("check");
                http.path = "/ready".to_owned();
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "readiness port name",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                let application_service_readiness::Check::HttpGet(http) = declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[1]
                    .readiness
                    .as_mut()
                    .expect("readiness")
                    .check
                    .as_mut()
                    .expect("check");
                http.port_name = "http-alt".to_owned();
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "reconfigurable environment",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration.inference_route_id = Some("route-id".to_owned());
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .services[0]
                    .configuration_bindings[0] = route_configuration_binding(
                    "config",
                    "CONFIG",
                    ApplicationInferenceRouteProjection::RouteName,
                );
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "publication endpoint",
            |declaration: &mut ApplicationDeploymentDeclaration| {
                declaration
                    .snapshot
                    .as_mut()
                    .expect("snapshot")
                    .graph
                    .as_mut()
                    .expect("graph")
                    .primary_endpoint
                    .as_mut()
                    .expect("primary endpoint")
                    .port_name = "http-alt".to_owned()
            },
            Ipv4Addr::LOCALHOST,
        ),
        (
            "publication host",
            |_declaration: &mut ApplicationDeploymentDeclaration| {},
            Ipv4Addr::new(127, 0, 0, 2),
        ),
    ];
    for (name, mutate, host) in changes {
        let mut changed = baseline.clone();
        mutate(&mut changed);
        synchronize_service_selections(&mut changed);
        assert_ne!(generation(&changed, host), baseline_generation, "{name}");
    }
}

#[test]
fn launch_gates_only_services_a_sibling_waits_on_with_ready() {
    let mut desired = deployment(ApplicationDeploymentDesiredState::Present);
    let graph = desired
        .snapshot
        .as_mut()
        .expect("snapshot")
        .graph
        .as_mut()
        .expect("graph");
    let readiness = |path: &str| {
        Some(ApplicationServiceReadiness {
            check: Some(application_service_readiness::Check::HttpGet(
                ApplicationHttpReadiness {
                    port_name: "http".to_owned(),
                    path: path.to_owned(),
                },
            )),
        })
    };
    let mut database = graph.services[0].clone();
    database.service_name = "database".to_owned();
    database.readiness = readiness("/ready");
    let mut cache = graph.services[0].clone();
    cache.service_name = "cache".to_owned();
    cache.readiness = readiness("/health");
    let web = &mut graph.services[0];
    web.readiness = readiness("/");
    web.dependencies = vec![
        ApplicationServiceDependency {
            service_name: "database".to_owned(),
            condition: ApplicationDependencyCondition::Ready as i32,
        },
        ApplicationServiceDependency {
            service_name: "cache".to_owned(),
            condition: ApplicationDependencyCondition::Started as i32,
        },
    ];
    graph.services.extend([database, cache]);
    synchronize_service_selections(&mut desired);

    let launch = workload_group_launch(
        &desired,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("valid runtime launch");

    let gates = launch
        .units
        .iter()
        .map(|(name, unit)| (name.0.as_str(), unit.ready_launch_gate.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        gates,
        vec![
            ("cache", None),
            (
                "database",
                Some(
                    dotcore_runtime::ReadyLaunchGate::http_get(
                        "http",
                        "/ready",
                        std::time::Duration::from_secs(30),
                        std::time::Duration::from_secs(1),
                    )
                    .expect("valid launch gate"),
                ),
            ),
            ("web", None),
        ]
    );
    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].dependencies,
        vec![
            dotcore_runtime::WorkloadUnitDependency {
                unit_name: WorkloadUnitName("database".to_owned()),
                condition: dotcore_runtime::WorkloadDependencyCondition::Ready,
            },
            dotcore_runtime::WorkloadUnitDependency {
                unit_name: WorkloadUnitName("cache".to_owned()),
                condition: dotcore_runtime::WorkloadDependencyCondition::Started,
            },
        ]
    );
}

#[test]
fn single_service_with_readiness_launches_without_a_gate() {
    let launch = workload_group_launch(
        &http_readiness_deployment(),
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("valid runtime launch");

    let web = &launch.units[&WorkloadUnitName("web".to_owned())];
    assert_eq!(launch.units.len(), 1);
    assert_eq!(web.ready_launch_gate, None);
    assert_eq!(web.dependencies, Vec::new());
}

#[test]
fn launch_rejects_a_graph_that_no_longer_matches_the_retained_selection() {
    let mut selected = deployment(ApplicationDeploymentDesiredState::Present);
    let reference = selected
        .snapshot
        .as_mut()
        .expect("Application snapshot")
        .graph
        .as_mut()
        .expect("Application graph")
        .services[0]
        .artifact
        .as_mut()
        .expect("OCI artifact")
        .oci_mut()
        .expect("OCI reference");
    reference.tag = None;
    reference.digest = None;

    let error = workload_group_launch(
        &selected,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect_err("graph and retained selection mismatch");

    assert_eq!(
        error.to_string(),
        "Application lifecycle configuration failed: Application service selections do not match the retained graph"
    );
}

#[tokio::test]
async fn invalid_persisted_oci_artifact_never_mutates_the_runtime() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut persisted = deployment(ApplicationDeploymentDesiredState::Present);
    let reference = persisted
        .snapshot
        .as_mut()
        .expect("Application snapshot")
        .graph
        .as_mut()
        .expect("Application graph")
        .services[0]
        .artifact
        .as_mut()
        .expect("OCI artifact")
        .oci_mut()
        .expect("OCI reference");
    reference.tag = None;
    reference.digest = None;
    desired.seed(persisted);
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application())
                .build()
                .expect("Catalog"),
        ),
        desired,
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    controller
        .realizer
        .reconcile()
        .await
        .expect("failed deployment projection");

    assert!(calls.lock().expect("Runtime calls").is_empty());
    assert_eq!(
        current.snapshot().expect("current state").deployments[0]
            .state
            .as_ref()
            .expect("failed deployment state")
            .stage,
        "launch-preparation"
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn unavailable_configuration_source_rejects_before_desired_or_runtime_mutation() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        test_catalog(),
        desired,
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    let result = ApplicationLifecycleController::create_deployment(
        &controller,
        CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        },
    )
    .await;

    assert!(matches!(
        result,
        Err(ApplicationLifecycleError::Configuration(source))
            if source.to_string() == "Application configuration source is unavailable"
    ));
    assert!(
        controller
            .desired
            .snapshot()
            .expect("desired state")
            .deployments
            .is_empty()
    );
    assert!(
        controller
            .current
            .snapshot()
            .expect("current state")
            .deployments
            .is_empty()
    );
    assert!(calls.lock().expect("Runtime calls").is_empty());
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn controller_rejects_retained_declarations_without_an_eligible_target() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
    declaration.placement = Some(ExactPlacement {
        site_node_id: "node-two".to_owned(),
        execution_target_id: "application-target-two".to_owned(),
    });
    desired.upsert(declaration, 0).expect("desired deployment");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .expect("Runtime adapter");

    let error = match TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_route_input())
                .build()
                .expect("Catalog"),
        ),
        desired,
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    ) {
        Ok(_) => panic!("declarations for an undeclared target must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        ApplicationLifecycleError::IneligibleTarget {
            reason: TargetIneligibility::Undeclared,
            ..
        }
    ));
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn create_rejects_site_node_identity_outside_the_identifier_rule() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_route_input())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    for site_node_id in ["Node-One", "node_one", ""] {
        let result = controller
            .create_deployment(CreateApplicationDeploymentRequest {
                application_id: "open-webui".to_owned(),
                placement: Some(ExactPlacement {
                    site_node_id: site_node_id.to_owned(),
                    execution_target_id: "application-target-one".to_owned(),
                }),
                inference_route_id: None,
                expected_inference_route_generation: 0,
                application_deployment_name: None,
                parameter_values: Vec::new(),
            })
            .await;
        assert!(
            matches!(result, Err(ApplicationLifecycleError::InvalidArgument)),
            "{site_node_id:?} must be rejected as an invalid node identity"
        );
    }
    assert_eq!(
        desired.snapshot().expect("desired state").deployments,
        Vec::new()
    );
    assert_eq!(
        current.snapshot().expect("current state").deployments,
        Vec::new()
    );
    assert!(calls.lock().expect("Runtime calls").is_empty());

    let created = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("deployment creation");

    assert_eq!(created.placement, Some(exact_placement()));
    assert_eq!(
        desired
            .snapshot()
            .expect("persisted declaration")
            .deployments[0]
            .placement,
        Some(exact_placement())
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[test]
fn only_the_exact_application_target_of_the_named_node_is_eligible() {
    let configured = site();

    assert!(eligible_target(&configured, &exact_placement()).is_ok());
    assert!(
        eligible_target(
            &configured,
            &ExactPlacement {
                site_node_id: "node-two".to_owned(),
                execution_target_id: "application-target-one".to_owned(),
            },
        )
        .is_err()
    );
    assert!(
        eligible_target(
            &configured,
            &ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: "unknown".to_owned(),
            },
        )
        .is_err()
    );
}

#[test]
fn deployment_failure_state_exposes_only_the_stable_stage() {
    let state = deployment_failure_state("runtime-creation", None);

    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );
    assert_eq!(
        state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(state.stage, "runtime-creation");
    assert_eq!(state.failure, None);
    assert_eq!(state.endpoint, None);
}

#[tokio::test]
async fn completed_negative_readiness_is_observation_while_malformed_endpoint_is_an_error() {
    let desired = http_readiness_deployment();
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("readiness listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("probe connection");
        crate::test_support::http::read_request(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await
            .expect("probe response");
    });

    assert!(
        !primary_endpoint_ready(&desired, &format!("http://{address}"))
            .await
            .expect("completed readiness probe")
    );
    server.await.expect("readiness server");
    for malformed in [
        format!("https://{address}"),
        "http://not-an-address".to_owned(),
    ] {
        assert!(
            primary_endpoint_ready(&desired, &malformed).await.is_err(),
            "{malformed} is a configuration error"
        );
    }
}

#[tokio::test]
async fn endpoint_that_refuses_resets_or_closes_before_answering_is_not_ready_yet() {
    let desired = http_readiness_deployment();
    let reserved = dotcore_domain::test_support::ports::reserve_port();
    let refused = std::net::SocketAddr::from((Ipv4Addr::LOCALHOST, reserved.port()));
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("readiness listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        // Accepts and resets, like Docker Desktop's port forwarder before the container listens.
        let (mut reset, _) = listener.accept().await.expect("reset probe connection");
        crate::test_support::http::read_request(&mut reset).await;
        reset.set_zero_linger().expect("abortive close");
        drop(reset);
        let (mut closed, _) = listener.accept().await.expect("closed probe connection");
        crate::test_support::http::read_request(&mut closed).await;
        drop(closed);
    });

    for (case, url) in [
        ("refused", format!("http://{refused}")),
        ("reset", format!("http://{address}")),
        ("closed", format!("http://{address}")),
    ] {
        assert!(
            !primary_endpoint_ready(&desired, &url)
                .await
                .unwrap_or_else(|error| panic!("{case} probe is not ready yet: {error:?}")),
            "{case} probe"
        );
    }
    server.await.expect("readiness server");
}

#[tokio::test]
async fn booting_group_starts_until_ready_and_becomes_unavailable_when_it_stops_serving() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let deployment = http_readiness_deployment();
    desired
        .upsert(deployment.clone(), 0)
        .expect("present desired deployment");
    let launch = workload_group_launch(
        &deployment,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch");
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .expect("readiness listener");
    let address = listener.local_addr().expect("listener address");
    let server = tokio::spawn(async move {
        for answers in [false, true, false] {
            let (mut stream, _) = listener.accept().await.expect("probe connection");
            crate::test_support::http::read_request(&mut stream).await;
            if answers {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .expect("probe response");
            }
        }
    });
    let mut running = group_observation(&launch, [("web", WorkloadUnitState::Running)], false);
    if let WorkloadGroupObservation::Observed(group) = &mut running {
        group.assigned_endpoint = Some(dotcore_runtime::AssignedWorkloadEndpoint {
            unit_name: WorkloadUnitName("web".to_owned()),
            port_name: "http".to_owned(),
            configured_host: Ipv4Addr::LOCALHOST,
            assigned_port: std::num::NonZeroU16::new(address.port()).expect("non-zero port"),
        });
    }
    let controller = controller_with_runtime(
        desired,
        current.clone(),
        PausedMatrixRuntime::new(vec![running], None, Arc::new(Mutex::new(Vec::new()))),
    );
    let state = || {
        current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("deployment state")
    };

    // Booting: the endpoint does not answer yet, so the deployment starts and is probed soon.
    let booting = controller.realizer.reconcile().await.expect("booting pass");
    assert_eq!(booting, ReconciliationState::Pending);
    let starting = state();
    assert_eq!(
        starting.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        starting.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(starting.stage, ENDPOINT_NOT_READY_STAGE);
    assert_eq!(starting.endpoint, None);

    // Ready once the endpoint answers.
    let ready = controller.realizer.reconcile().await.expect("ready pass");
    assert_eq!(ready, ReconciliationState::Settled);
    let endpoint = Some(ApplicationEndpoint {
        url: format!("http://{address}"),
    });
    assert_eq!(
        state().lifecycle_state,
        ApplicationLifecycleState::Ready as i32
    );
    assert_eq!(state().endpoint, endpoint);

    // Lost readiness after being ready in this start is a fault.
    let lost = controller.realizer.reconcile().await.expect("lost pass");
    assert_eq!(lost, ReconciliationState::Pending);
    let unavailable = state();
    assert_eq!(
        unavailable.lifecycle_state,
        ApplicationLifecycleState::Unavailable as i32
    );
    assert_eq!(unavailable.stage, ENDPOINT_NOT_READY_STAGE);
    assert_eq!(unavailable.endpoint, endpoint);

    server.await.expect("readiness server");
    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn creation_failure_is_local_redacted_logged_retried_and_does_not_block_later_work() {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    let first = deployment(ApplicationDeploymentDesiredState::Present);
    let mut second = first.clone();
    second.application_deployment_id = "open-webui-2".to_owned();
    second.application_deployment_name = "open-webui-2".to_owned();
    desired.upsert(first.clone(), 0).expect("first desired");
    desired.upsert(second.clone(), 1).expect("second desired");
    for value in [&first, &second] {
        current
            .upsert(
                CurrentApplicationDeployment {
                    application_deployment_id: value.application_deployment_id.clone(),
                    record_revision: 0,
                    state: Some(queued_state()),
                    reconciled_lifecycle_generation: None,
                },
                None,
            )
            .expect("current deployment");
    }
    let failures = Arc::new(Mutex::new(BTreeSet::from([first
        .application_deployment_id
        .clone()])));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let missing_images = Arc::new(Mutex::new(BTreeSet::new()));
    let taken_ports = Arc::new(Mutex::new(BTreeSet::new()));
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            SelectiveCreateRuntime {
                failures: failures.clone(),
                missing_images: missing_images.clone(),
                taken_ports: taken_ports.clone(),
                calls: calls.clone(),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        test_catalog(),
        desired,
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::new(
                dotcore_domain::SiteNodeHost::parse("192.0.2.10").expect("manager host"),
                6470,
            ),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");

    let (result, records, logs) = capture_records_async(|| controller.realizer.reconcile()).await;

    result.expect("deployment-local failure does not fail reconciliation");
    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "WARN"
                && fields.get("event_kind")
                    == Some(&"\"application_reconciliation_retryable_failure\"".to_owned())
                && fields.get("deployment_id") == Some(&"open-webui-1".to_owned())
                && fields.get("outcome") == Some(&"\"failed\"".to_owned())
    )));
    assert_eq!(
        calls.lock().expect("Runtime calls").as_slice(),
        ["open-webui-1", "open-webui-2"]
    );
    let mixed = controller.list_deployments().await.expect("mixed list");
    assert_eq!(mixed.deployments.len(), 2);
    let failed = controller
        .get_deployment(first.application_deployment_id.clone())
        .await
        .expect("failed deployment remains readable");
    assert_eq!(
        failed.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    let failed_state = failed.state.as_ref().expect("state");
    assert_eq!(
        failed_state.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        failed_state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(failed_state.stage, "starting");
    assert_eq!(
        failed_state.services,
        vec![ApplicationServiceObservation {
            service_name: "web".to_owned(),
            lifecycle_state: ApplicationServiceLifecycleState::Pending as i32,
            readiness_state: ApplicationReadinessState::NotReady as i32,
        }]
    );
    assert_eq!(
        failed_state.failure.as_ref().expect("safe failure").code,
        "CreationFailed"
    );
    assert_eq!(
        failed_state.failure.as_ref().expect("safe failure").message,
        "Application workload creation failed"
    );
    let public = format!("{failed:?}");
    assert!(!public.contains("secret-runtime-detail"));
    assert!(!public.contains("/var/lib/docker"));
    assert!(logs.contains("open-webui-1"));
    assert!(logs.contains("runtime-creation"));
    assert!(logs.contains("secret-runtime-detail"));
    assert!(logs.contains("/var/lib/docker/container-deadbeef"));
    let created = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("unrelated deployment failure does not block create");
    assert_eq!(
        created.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );

    failures.lock().expect("failures").clear();
    controller
        .realizer
        .reconcile()
        .await
        .expect("retry reconciliation");
    let retried = controller
        .get_deployment(first.application_deployment_id.clone())
        .await
        .expect("retried deployment")
        .state
        .expect("state");
    assert_eq!(
        retried.stage,
        "Application primary endpoint is not assigned on the configured host"
    );
    assert_eq!(retried.failure, None);

    missing_images
        .lock()
        .expect("missing images")
        .insert(first.application_deployment_id.clone());
    controller
        .realizer
        .reconcile()
        .await
        .expect("missing-image reconciliation");
    let missing_image = controller
        .get_deployment(first.application_deployment_id.clone())
        .await
        .expect("missing image deployment")
        .state
        .expect("state");
    assert_eq!(
        missing_image.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        missing_image.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(missing_image.stage, "starting");
    assert_eq!(
        missing_image.failure.as_ref().expect("safe failure").code,
        "ImageMissing"
    );
    assert_eq!(
        missing_image
            .failure
            .as_ref()
            .expect("safe failure")
            .message,
        "Application image is not available locally"
    );
    assert!(
        missing_image
            .failure
            .expect("safe failure")
            .diagnostics
            .is_empty()
    );

    missing_images.lock().expect("missing images").clear();
    controller
        .realizer
        .reconcile()
        .await
        .expect("missing-image recovery reconciliation");
    let recovered = controller
        .get_deployment(first.application_deployment_id.clone())
        .await
        .expect("recovered deployment")
        .state
        .expect("state");
    assert_eq!(
        recovered.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        recovered.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert_eq!(
        recovered.stage,
        "Application primary endpoint is not assigned on the configured host"
    );
    assert_eq!(recovered.failure, None);

    taken_ports
        .lock()
        .expect("taken ports")
        .insert(first.application_deployment_id.clone());
    controller
        .realizer
        .reconcile()
        .await
        .expect("taken-port reconciliation");
    let taken_port = controller
        .get_deployment(first.application_deployment_id.clone())
        .await
        .expect("taken port deployment")
        .state
        .expect("state");
    assert_eq!(taken_port.stage, "starting");
    assert_eq!(
        taken_port.failure.expect("safe failure"),
        ApplicationDeploymentFailure {
            code: "PublishedPortInUse".to_owned(),
            message: "Application published port is in use on this node; free the port or delete the deployment".to_owned(),
            diagnostics: Vec::new(),
        }
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn create_accepts_a_fresh_node_target_and_rejects_ineligible_pairs_with_their_reason() {
    let directory = crate::test_support::canonical_tempdir().expect("Site directory");
    let (desired, current) = open_node_site_with(directory.path(), |site| {
        let mut stale = application_execution_target();
        stale.execution_target_id = "application-target-two".to_owned();
        stale.display_name = "application-target-two".to_owned();
        let mut unobserved = application_execution_target();
        unobserved.execution_target_id = "application-target-three".to_owned();
        unobserved.display_name = "application-target-three".to_owned();
        let mut model = application_execution_target();
        model.execution_target_id = "model-target-one".to_owned();
        model.display_name = "model-target-one".to_owned();
        model.runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
            dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                adapter: Some(
                    dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::SimulatedRuntime(
                        dotcore_proto::dotcore::site::v1::SimulatedRuntimeAdapter {
                            default_scenario: Some(Default::default()),
                            ..Default::default()
                        },
                    ),
                ),
            },
        ));
        site.execution_targets.extend([stale, unobserved, model]);
    });
    let capabilities = TargetCapabilityReconciler::reconcile(
        [
            ("manager-target", "manager-node"),
            ("application-target-one", "node-one"),
            ("application-target-two", "node-one"),
        ],
        [
            RuntimePlatformObservation::available(
                "manager-target",
                dotcore_domain::ExecutionPlatform::linux_amd64(),
            ),
            RuntimePlatformObservation::available(
                "application-target-one",
                dotcore_domain::ExecutionPlatform::linux_amd64(),
            ),
            RuntimePlatformObservation::stale(
                "application-target-two",
                dotcore_domain::ExecutionPlatform::linux_amd64(),
            ),
        ],
        std::iter::empty(),
    );
    let controller = manager_controller(
        desired.clone(),
        current,
        Arc::new(StaticTargetCapabilitySource::new(capabilities)),
    );
    let create = |application_id: &str, execution_target_id: &str| {
        controller.create_deployment(CreateApplicationDeploymentRequest {
            application_id: application_id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: execution_target_id.to_owned(),
            }),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
    };

    let created = create("open-webui", "application-target-one")
        .await
        .expect("fresh node target");
    assert_eq!(created.placement, Some(exact_placement()));
    assert_eq!(
        created.state.as_ref().expect("state").lifecycle_state,
        ApplicationLifecycleState::Queued as i32
    );
    let local = create("local-webui", "application-target-one")
        .await
        .expect("tag-only service left for the node to resolve");
    let selections = &local
        .snapshot
        .as_ref()
        .expect("snapshot")
        .service_selections;
    assert!(selections.iter().all(|selection| {
        selection.launch_identity.is_empty()
            && selection.preparation_policy == ArtifactPreparationPolicy::LocalDevelopment as i32
    }));
    assert_eq!(
        ApplicationDeclarationRepository::snapshot(&desired)
            .expect("Site")
            .deployments
            .len(),
        2,
        "both node deployments are retained as validated declarations"
    );

    for (execution_target_id, reason) in [
        ("missing-target", TargetIneligibility::Undeclared),
        ("manager-target", TargetIneligibility::OwnedByAnotherNode),
        (
            "model-target-one",
            TargetIneligibility::NotApplicationTarget,
        ),
    ] {
        let error = create("open-webui", execution_target_id)
            .await
            .expect_err("ineligible pair");
        assert!(
            matches!(
                &error,
                ApplicationLifecycleError::IneligibleTarget { reason: actual, .. } if *actual == reason
            ),
            "{execution_target_id}: {error}"
        );
    }
    assert_eq!(
        create("open-webui", "manager-target")
            .await
            .expect_err("mismatched pair")
            .to_string(),
        "Application execution target \"manager-target\" on node \"node-one\" is not eligible: the execution target belongs to another node"
    );
    assert!(matches!(
        create("open-webui", "application-target-two").await,
        Err(ApplicationLifecycleError::StaleTargetObservation { .. })
    ));
    assert!(matches!(
        create("open-webui", "application-target-three").await,
        Err(ApplicationLifecycleError::MissingTargetObservation { .. })
    ));
    assert_eq!(
        ApplicationDeclarationRepository::snapshot(&desired)
            .expect("Site")
            .deployments
            .len(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn node_deployments_follow_node_status_and_stay_pending_while_disconnected() {
    let directory = crate::test_support::canonical_tempdir().expect("Site directory");
    let (desired, current) = open_node_site_with(directory.path(), |_| {});
    let controller =
        manager_controller(desired.clone(), current.clone(), node_target_capabilities());
    let mut node = ConnectedWorker::new(desired.clone(), current.clone());
    controller.set_remote_status_source(node.coordinator.clone());

    let created = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("node deployment");
    let id = created.application_deployment_id.clone();
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (
            listed.lifecycle_state,
            listed.stage.as_str(),
            listed.observation_stale
        ),
        (ApplicationLifecycleState::Queued as i32, "queued", true),
        "no node has reported yet"
    );

    node.connect();
    let view = node.publish();
    let ready = node_ready(&id, 1, &view);
    node.report(view, vec![ready], Vec::new());
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Ready as i32, false)
    );
    assert_eq!(
        listed
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.url.as_str()),
        Some("http://10.0.0.2:41000")
    );
    let assignments = node.assignments();

    // A manager restart rebuilds the same view from persisted intent and keeps the last
    // observation, stale until the node reports to the new manager session.
    drop((controller, node));
    drop((desired, current));
    let desired = SiteRepository::load(
        &directory.path().join("site.yaml"),
        crate::test_support::site_declaration_rules(),
    )
    .expect("Site reload");
    let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
        .expect("current state reload");
    let controller =
        manager_controller(desired.clone(), current.clone(), node_target_capabilities());
    let mut node = ConnectedWorker::new(desired.clone(), current.clone());
    controller.set_remote_status_source(node.coordinator.clone());
    assert_eq!(node.assignments(), assignments);
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Ready as i32, true)
    );
    node.connect();
    let view = node.publish();
    let ready = node_ready(&id, 1, &view);
    node.report(view, vec![ready], Vec::new());
    assert!(!listed_state(&controller, &id).await.observation_stale);

    // A disconnected node keeps its last observation, marked stale.
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Ready as i32, true)
    );
    assert!(listed.endpoint.is_some());

    // Pause while disconnected stays pending, then resolves on reconnect.
    let paused = controller
        .pause_deployment(id.clone())
        .await
        .expect("pause intent");
    assert_eq!(
        paused.state.expect("state").lifecycle_state,
        ApplicationLifecycleState::Pausing as i32
    );
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Pausing as i32, true)
    );
    node.connect();
    let view = node.publish();
    let mut paused = node_status(&id, 2, ApplicationLifecycleObservation::Paused, &view);
    paused.stage = ApplicationStatusStage::Paused as i32;
    node.report(view, vec![paused], Vec::new());
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Paused as i32, false)
    );

    // Teardown while disconnected stays pending until the node's cleanup evidence arrives.
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    controller
        .remove_deployment(id.clone())
        .await
        .expect("teardown intent");
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Stopping as i32, true)
    );
    node.connect();
    let view = node.publish();
    let receipts = node.report(
        view,
        Vec::new(),
        vec![application_cleanup_evidence(&id, 3, Some(1))],
    );
    assert_eq!(
        receipts,
        [dotcore_proto::dotcore::application::private::v1::ApplicationCleanupDisposition::Acknowledged
            as i32]
    );
    assert_eq!(
        controller
            .list_deployments()
            .await
            .expect("list")
            .deployments,
        Vec::new()
    );
    assert!(
        ApplicationDeclarationRepository::snapshot(&desired)
            .expect("Site")
            .deployments
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn node_deployments_stay_stale_until_the_current_node_process_lists_them() {
    let directory = crate::test_support::canonical_tempdir().expect("site directory");
    let (desired, current) = open_node_site_with(directory.path(), |_| {});
    let controller =
        manager_controller(desired.clone(), current.clone(), node_target_capabilities());
    let mut node = ConnectedWorker::new(desired.clone(), current.clone());
    controller.set_remote_status_source(node.coordinator.clone());
    let id = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("node deployment")
        .application_deployment_id;
    node.connect();
    let view = node.publish();
    let ready = node_ready(&id, 1, &view);
    node.report(view, vec![ready], Vec::new());
    assert!(!listed_state(&controller, &id).await.observation_stale);

    // A restarted node process reports before it has listed the deployment: the record the
    // earlier process reported is not current.
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    node.incarnation = NodeIncarnation::parse("2".repeat(32)).expect("node incarnation");
    node.connect();
    let view = node.publish();
    node.report(view.clone(), Vec::new(), Vec::new());
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Ready as i32, true)
    );
    assert_eq!(
        controller
            .list_deployments()
            .await
            .expect("list")
            .deployments
            .into_iter()
            .map(|deployment| deployment.state.expect("state"))
            .collect::<Vec<_>>(),
        vec![listed],
        "list and get project the same state"
    );

    let ready = node_ready(&id, 1, &view);
    node.report(view, vec![ready], Vec::new());
    assert!(!listed_state(&controller, &id).await.observation_stale);

    // A record an accepted mutation wrote stands in for the node's acknowledgement.
    controller
        .pause_deployment(id.clone())
        .await
        .expect("pause intent");
    let listed = listed_state(&controller, &id).await;
    assert_eq!(
        (listed.lifecycle_state, listed.observation_stale),
        (ApplicationLifecycleState::Pausing as i32, false)
    );
}

#[tokio::test(start_paused = true)]
async fn unit_memory_is_read_from_current_node_status_and_unavailable_otherwise() {
    let directory = crate::test_support::canonical_tempdir().expect("site directory");
    let (desired, current) = open_node_site_with(directory.path(), |_| {});
    let controller =
        manager_controller(desired.clone(), current.clone(), node_target_capabilities());
    let mut node = ConnectedWorker::new(desired.clone(), current.clone());
    controller.set_remote_status_source(node.coordinator.clone());
    let id = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("node deployment")
        .application_deployment_id;
    let reported = |used_memory_bytes| AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet {
                devices: vec![AcceleratorMemoryDeviceUsage {
                    managed_node_id: "node-one".to_owned(),
                    device_id: "GPU-a".to_owned(),
                    used_memory_bytes,
                }],
            },
        )),
        ..Default::default()
    };
    let node_status_unavailable = AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: "NODE_STATUS_UNAVAILABLE".to_owned(),
                message: "The hosting node has not reported current status for this deployment."
                    .to_owned(),
            },
        )),
        ..Default::default()
    };
    let unit = |memory: &AcceleratorMemoryObservation| {
        vec![ApplicationServiceAcceleratorMemory {
            service_name: "web".to_owned(),
            accelerator_memory: Some(memory.clone()),
        }]
    };
    let ready_with_memory =
        |view: &dotcore_proto::dotcore::site::private::v1::NodeViewReference, bytes| {
            let mut ready = node_ready(&id, 1, view);
            ready.service_accelerator_memory = unit(&reported(bytes));
            ready
        };
    let listed_memory = async |controller: &ApplicationController<_, _>| {
        controller
            .get_deployment(id.clone())
            .await
            .expect("listed deployment")
            .service_accelerator_memory
    };
    assert_eq!(
        listed_memory(&controller).await,
        unit(&node_status_unavailable),
        "no node has reported yet"
    );

    // Each current report replaces the projected memory without touching the record.
    node.connect();
    let view = node.publish();
    node.report(
        view.clone(),
        vec![ready_with_memory(&view, 1000)],
        Vec::new(),
    );
    assert_eq!(listed_memory(&controller).await, unit(&reported(1000)));
    let record = || {
        CurrentApplicationRepository::snapshot(&current)
            .expect("current state")
            .deployments
            .into_iter()
            .find(|record| record.application_deployment_id == id)
            .expect("current record")
    };
    let before = record();
    node.report(
        view.clone(),
        vec![ready_with_memory(&view, 2000)],
        Vec::new(),
    );
    assert_eq!(listed_memory(&controller).await, unit(&reported(2000)));
    assert_eq!(record(), before);

    // A silent node's last report is no longer evidence.
    tokio::time::advance(std::time::Duration::from_secs(15)).await;
    assert_eq!(
        listed_memory(&controller).await,
        unit(&node_status_unavailable)
    );

    // A restarted manager reads unavailable until the node reports to it.
    drop((controller, node));
    drop((desired, current));
    let desired = SiteRepository::load(
        &directory.path().join("site.yaml"),
        crate::test_support::site_declaration_rules(),
    )
    .expect("site reload");
    let current = FileCurrentState::open(directory.path().join("site.yaml"), "site-one")
        .expect("current state reload");
    let controller =
        manager_controller(desired.clone(), current.clone(), node_target_capabilities());
    let mut node = ConnectedWorker::new(desired, current);
    controller.set_remote_status_source(node.coordinator.clone());
    assert_eq!(
        listed_memory(&controller).await,
        unit(&node_status_unavailable)
    );
    node.connect();
    let view = node.publish();
    node.report(
        view.clone(),
        vec![ready_with_memory(&view, 3000)],
        Vec::new(),
    );
    assert_eq!(listed_memory(&controller).await, unit(&reported(3000)));
    assert_eq!(
        controller
            .list_deployments()
            .await
            .expect("list")
            .deployments[0]
            .service_accelerator_memory,
        unit(&reported(3000)),
        "list and get project the same memory"
    );
}

#[tokio::test]
async fn deployment_acceptance_distinguishes_target_observation_failures() {
    let missing = TargetCapabilityInventory::default();
    let unavailable = TargetCapabilityReconciler::reconcile(
        [("application-target-one", "node-one")],
        [RuntimePlatformObservation::unavailable(
            "application-target-one",
        )],
        std::iter::empty(),
    );
    let stale = TargetCapabilityReconciler::reconcile(
        [("application-target-one", "node-one")],
        [RuntimePlatformObservation::stale(
            "application-target-one",
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        )],
        std::iter::empty(),
    );
    let mismatched = TargetCapabilityReconciler::reconcile(
        [("application-target-one", "node-one")],
        [RuntimePlatformObservation::available(
            "application-target-one",
            dotcore_domain::ExecutionPlatform::linux_arm64(),
        )],
        std::iter::empty(),
    );

    for (inventory, expected) in [
        (missing, "missing"),
        (unavailable, "unavailable"),
        (stale, "stale"),
        (mismatched, "mismatched"),
    ] {
        let controller = controller_for_target_inventory(application(), inventory);
        let error = controller
            .create_deployment(CreateApplicationDeploymentRequest {
                application_id: "open-webui".to_owned(),
                placement: Some(exact_placement()),
                ..Default::default()
            })
            .await
            .expect_err("target observation rejection");
        assert!(
            matches!(
                (&error, expected),
                (
                    ApplicationLifecycleError::MissingTargetObservation { .. },
                    "missing"
                ) | (
                    ApplicationLifecycleError::UnavailableTargetObservation { .. },
                    "unavailable"
                ) | (
                    ApplicationLifecycleError::StaleTargetObservation { .. },
                    "stale"
                ) | (
                    ApplicationLifecycleError::TargetPlatformMismatch { .. },
                    "mismatched"
                )
            ),
            "unexpected {expected} error: {error:?}"
        );
    }
}

#[tokio::test]
async fn deployment_acceptance_reports_the_incompatible_service_before_persistence() {
    let mut incompatible = application();
    incompatible.graph.as_mut().expect("graph").services[0]
        .artifact
        .as_mut()
        .expect("artifact")
        .supported_platforms[0]
        .architecture = "arm64".to_owned();
    let desired = TestApplicationDeclarationRepository::open(Path::new("."), "site-one")
        .expect("declaration repository");
    let controller = controller_for_application_and_target_source(
        incompatible,
        desired.clone(),
        test_target_capability_source(),
    );

    let error = controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            ..Default::default()
        })
        .await
        .expect_err("unsupported service platform");

    assert!(matches!(
        error,
        ApplicationLifecycleError::UnsupportedServicePlatform {
            service_name,
            platform,
        } if service_name == "web" && platform == "linux/amd64"
    ));
    assert_eq!(
        desired.snapshot().expect("desired snapshot").deployments,
        Vec::new()
    );
}

#[test]
fn controller_restart_rejects_a_retained_deployment_whose_target_is_undeclared() {
    let desired = TestApplicationDeclarationRepository::open(Path::new("."), "site-one")
        .expect("declaration repository");
    let mut retained = deployment(ApplicationDeploymentDesiredState::Present);
    retained
        .placement
        .as_mut()
        .expect("placement")
        .execution_target_id = "undeclared-target".to_owned();
    desired.seed(retained);

    let result = ApplicationController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application())
                .build()
                .expect("Catalog"),
        ),
        desired,
        InMemoryCurrentState::new("site-one").expect("current repository"),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    );

    assert!(matches!(
        result,
        Err(ApplicationLifecycleError::IneligibleTarget {
            reason: TargetIneligibility::Undeclared,
            ..
        })
    ));
}

#[tokio::test]
async fn controller_restart_reports_a_retained_platform_change_on_the_deployment() {
    let outcome = reconcile_after_target_change(docker_application_target(Vec::new()), |target| {
        target.platform = Some(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform());
    })
    .await;

    outcome.assert_target_incompatible();
    assert!(matches!(
        validate_retained_target(&outcome.retained, &outcome.changed_site),
        Err(ApplicationLifecycleError::RetainedTargetIncompatible(
            RetainedTargetMismatch::Platform
        ))
    ));
}

#[tokio::test]
async fn controller_restart_reports_a_retained_device_change_on_the_deployment() {
    let outcome = reconcile_after_target_change(docker_application_target(Vec::new()), |target| {
        target.accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
    })
    .await;

    outcome.assert_target_incompatible();
    assert!(matches!(
        validate_retained_target(&outcome.retained, &outcome.changed_site),
        Err(ApplicationLifecycleError::RetainedTargetIncompatible(
            RetainedTargetMismatch::Devices
        ))
    ));
}

#[test]
fn amd64_and_arm64_select_the_same_immutable_application_reference() {
    let mut portable = application();
    let artifact = portable.graph.as_mut().expect("graph").services[0]
        .artifact
        .as_mut()
        .expect("artifact");
    artifact
        .supported_platforms
        .push(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform());
    let amd64 = resolve_target_snapshot(&portable, &site(), "application-target-one")
        .expect("amd64 snapshot");
    let mut arm64_site = site();
    arm64_site.execution_targets[0].platform =
        Some(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform());
    let arm64 = resolve_target_snapshot(&portable, &arm64_site, "application-target-one")
        .expect("arm64 snapshot");

    assert_eq!(
        amd64.service_selections[0].launch_identity,
        arm64.service_selections[0].launch_identity
    );
    assert_eq!(
        amd64.platform,
        Some(dotcore_domain::ExecutionPlatform::linux_amd64().to_oci_platform())
    );
    assert_eq!(
        arm64.platform,
        Some(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform())
    );
}

#[test]
fn snapshot_selection_rejects_a_service_that_does_not_support_the_group_platform() {
    let mut arm64_site = site();
    arm64_site.execution_targets[0]
        .platform
        .as_mut()
        .expect("target platform")
        .architecture = "arm64".to_owned();

    assert!(matches!(
        resolve_target_snapshot(&application(), &arm64_site, "application-target-one"),
        Err(ApplicationLifecycleError::UnsupportedServicePlatform {
            service_name,
            platform,
        }) if service_name == "web" && platform == "linux/arm64"
    ));
}

#[test]
fn snapshot_selection_leaves_a_docker_delivered_nix_service_to_the_hosting_node() {
    let mut application = application();
    let artifact = application.graph.as_mut().expect("graph").services[0]
        .artifact
        .as_mut()
        .expect("artifact");
    artifact.kind = Some(
        dotcore_proto::dotcore::catalog::v1::application_service_artifact::Kind::Nix(
            dotcore_proto::dotcore::catalog::v1::NixArtifact {
                flake: "self".to_owned(),
                package: Some("apps.web.docker".to_owned()),
                provides: Some(
                    dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::DockerImage(
                        dotcore_proto::dotcore::catalog::v1::NixDockerImage {},
                    ),
                ),
                store_paths: Vec::new(),
            },
        ),
    );

    let snapshot = resolve_target_snapshot(&application, &site(), "application-target-one")
        .expect("Docker-delivered snapshot");

    let selection = snapshot
        .service_selections
        .iter()
        .find(|selection| selection.service_name == "web")
        .expect("web selection");
    assert_eq!(
        selection.preparation_policy,
        ArtifactPreparationPolicy::DevelopmentSource as i32
    );
    assert_eq!(selection.launch_identity, "");
    assert_eq!(
        crate::application::repository::selection_pin_source(&snapshot, selection),
        Some(
            dotcore_domain::deployment::launch_pin::PinSource::NixDevelopmentSource {
                package: "apps.web.docker".to_owned(),
                docker_image: true,
            }
        )
    );
}

#[test]
fn a_locked_nix_service_launches_only_a_pin_among_its_catalog_store_paths() {
    const RETAINED: &str = "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
    const OTHER: &str = "/nix/store/1123456789abcdfghijklmnpqrsvwxyz-docker-image-web";
    let launch_with = |launch_identity: &str| {
        let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
        let snapshot = declaration.snapshot.as_mut().expect("snapshot");
        snapshot.graph.as_mut().expect("graph").services[0]
            .artifact
            .as_mut()
            .expect("artifact")
            .kind = Some(
            dotcore_proto::dotcore::catalog::v1::application_service_artifact::Kind::Nix(
                dotcore_proto::dotcore::catalog::v1::NixArtifact {
                    flake: dotcore_domain::test_support::nix::locked_flake(),
                    package: Some("apps.web.docker".to_owned()),
                    provides: Some(
                        dotcore_proto::dotcore::catalog::v1::nix_artifact::Provides::DockerImage(
                            dotcore_proto::dotcore::catalog::v1::NixDockerImage {},
                        ),
                    ),
                    store_paths: vec![dotcore_proto::dotcore::catalog::v1::NixSystemStorePath {
                        system: "x86_64-linux".to_owned(),
                        store_path: RETAINED.to_owned(),
                    }],
                },
            ),
        );
        let selection = &mut snapshot.service_selections[0];
        selection.preparation_policy = ArtifactPreparationPolicy::ImmutableRemote as i32;
        selection.launch_identity = launch_identity.to_owned();
        workload_group_launch(
            &declaration,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
    };

    let launch = launch_with(RETAINED).expect("retained pin launches");
    assert_eq!(
        launch
            .units
            .get(&WorkloadUnitName("web".to_owned()))
            .expect("web service")
            .image,
        dotcore_domain::nix::docker_launch_reference(RETAINED).expect("reference")
    );
    assert!(
        launch_with(OTHER).is_err(),
        "a pin outside the catalog store paths never launches"
    );
}

#[test]
fn retained_service_selection_is_the_only_runtime_image_authority() {
    let mut declaration = deployment(ApplicationDeploymentDesiredState::Present);
    let snapshot = declaration.snapshot.as_mut().expect("snapshot");
    snapshot.service_selections[0].launch_identity =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();

    let launch = workload_group_launch(
        &declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("Runtime launch");

    assert_eq!(launch.execution_target_id, "application-target-one");
    assert_eq!(
        launch.required_platform,
        dotcore_domain::ExecutionPlatform::linux_amd64()
    );
    assert_eq!(
        launch
            .units
            .get(&WorkloadUnitName("web".to_owned()))
            .expect("web service")
            .image,
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
}

#[test]
fn retained_target_platform_and_launch_identity_define_canonical_generation() {
    let baseline = deployment(ApplicationDeploymentDesiredState::Present);
    let generation = |declaration: &ApplicationDeploymentDeclaration| {
        workload_group_launch(
            declaration,
            Ipv4Addr::LOCALHOST,
            test_inference_route_resolver().as_ref(),
        )
        .expect("Runtime launch")
        .correlation
        .reconcile_generation
    };
    let baseline_generation = generation(&baseline);

    let mut changed_target = baseline.clone();
    changed_target
        .snapshot
        .as_mut()
        .expect("snapshot")
        .placement
        .as_mut()
        .expect("placement")
        .execution_target_id = "application-target-two".to_owned();
    assert_ne!(generation(&changed_target), baseline_generation);

    let mut changed_platform = baseline.clone();
    changed_platform
        .snapshot
        .as_mut()
        .expect("snapshot")
        .platform = Some(dotcore_domain::ExecutionPlatform::linux_arm64().to_oci_platform());
    assert_ne!(generation(&changed_platform), baseline_generation);

    let mut changed_identity = baseline.clone();
    changed_identity
        .snapshot
        .as_mut()
        .expect("snapshot")
        .service_selections[0]
        .launch_identity =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
    assert_ne!(generation(&changed_identity), baseline_generation);

    let mut changed_devices = baseline.clone();
    changed_devices
        .snapshot
        .as_mut()
        .expect("snapshot")
        .accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
    assert_ne!(generation(&changed_devices), baseline_generation);
}

#[test]
fn retained_device_set_reaches_the_group_launch_and_keeps_the_device_less_wire_form() {
    let declaration = deployment(ApplicationDeploymentDesiredState::Present);
    let deviceless_bytes = prost::Message::encode_to_vec(&declaration);

    // An explicitly empty retained set serializes exactly like the omitted field, so a
    // device-less deployment keeps the declaration bytes it stored.
    let mut explicit_empty = declaration.clone();
    explicit_empty
        .snapshot
        .as_mut()
        .expect("snapshot")
        .accelerator_device_ids = Vec::new();
    assert_eq!(
        prost::Message::encode_to_vec(&explicit_empty),
        deviceless_bytes
    );

    let deviceless_launch = workload_group_launch(
        &declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("device-less launch");
    assert!(
        deviceless_launch
            .execution_target
            .accelerator_device_ids
            .is_empty()
    );

    let mut gpus = declaration;
    gpus.snapshot
        .as_mut()
        .expect("snapshot")
        .accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
    let launch = workload_group_launch(
        &gpus,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("launch with retained devices");
    assert_eq!(
        launch.execution_target.accelerator_device_ids,
        vec!["nvidia.com/gpu=0".to_owned()]
    );
}

#[tokio::test]
async fn retained_target_recheck_fails_a_deployment_whose_target_devices_changed() {
    let outcome = reconcile_after_target_device_change(
        docker_application_target(vec!["nvidia.com/gpu=0".to_owned()]),
        vec!["nvidia.com/gpu=1".to_owned()],
    )
    .await;

    outcome.assert_incompatible();
}

#[tokio::test]
async fn retained_target_recheck_fails_a_deployment_whose_target_devices_were_removed() {
    let outcome = reconcile_after_target_device_change(
        docker_application_target(vec!["nvidia.com/gpu=0".to_owned()]),
        Vec::new(),
    )
    .await;

    outcome.assert_incompatible();
}

#[tokio::test]
async fn retained_target_recheck_spares_a_simulated_target_device_change() {
    let outcome = reconcile_after_target_device_change(
        application_execution_target(),
        vec!["nvidia.com/gpu=0".to_owned()],
    )
    .await;

    assert_eq!(outcome.retained_devices, Vec::<String>::new());
    assert_eq!(outcome.after.stage, outcome.before.stage);
    assert_eq!(outcome.after.failure, None);
    assert_eq!(outcome.calls_after_change, outcome.calls_before_change);
}

/// Creates a deployment on `target`, reconciles it once, changes the target's configured devices
/// to `changed_devices`, and reconciles again.
async fn reconcile_after_target_device_change(
    target: ExecutionTarget,
    changed_devices: Vec<String>,
) -> TargetChangeOutcome {
    reconcile_after_target_change(target, |target| {
        target.accelerator_device_ids = changed_devices;
    })
    .await
}

/// Creates a deployment on `target`, reconciles it once, applies `change` to the declared target,
/// and reconciles again.
async fn reconcile_after_target_change(
    target: ExecutionTarget,
    change: impl FnOnce(&mut ExecutionTarget),
) -> TargetChangeOutcome {
    let directory = test_directory();
    let desired = TestApplicationDeclarationRepository::open(&directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(&directory, "site-one").expect("current repository");
    desired.set_execution_target(target.clone());
    let runtime = RetainedGroupRuntime::default();
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert("application-target-one", runtime.clone())
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("open-webui", application())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current.clone(),
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");
    let state = |current: &InMemoryCurrentState| {
        current.snapshot().expect("current state").deployments[0]
            .state
            .clone()
            .expect("deployment state")
    };

    controller
        .create_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(exact_placement()),
            ..Default::default()
        })
        .await
        .expect("deployment creation");
    let retained_devices = desired.snapshot().expect("desired state").deployments[0]
        .snapshot
        .as_ref()
        .expect("snapshot")
        .accelerator_device_ids
        .clone();
    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation");
    let before = state(&current);
    let calls_before_change = runtime.calls();

    let retained = desired.snapshot().expect("desired state").deployments[0].clone();
    let mut changed = target;
    change(&mut changed);
    let changed_site = Site {
        execution_targets: vec![changed.clone()],
        ..site()
    };
    desired.set_execution_target(changed);
    controller
        .realizer
        .reconcile()
        .await
        .expect("reconciliation after the device change");
    let after = state(&current);
    let calls_after_change = runtime.calls();

    drop(controller);
    fs::remove_dir_all(directory).expect("test directory cleanup");
    TargetChangeOutcome {
        retained,
        changed_site,
        retained_devices,
        before,
        after,
        calls_before_change,
        calls_after_change,
    }
}

struct TargetChangeOutcome {
    retained: ApplicationDeploymentDeclaration,
    changed_site: Site,
    retained_devices: Vec<String>,
    before: ApplicationDeploymentState,
    after: ApplicationDeploymentState,
    calls_before_change: Vec<(&'static str, dotcore_runtime::WorkloadGroupLaunch)>,
    calls_after_change: Vec<(&'static str, dotcore_runtime::WorkloadGroupLaunch)>,
}

impl TargetChangeOutcome {
    /// Asserts the second pass reported the target-compatibility failure without touching the
    /// runtime: lifecycle `Unavailable`, and no group created or removed after the first pass.
    fn assert_target_incompatible(&self) {
        assert_ne!(self.before.stage, "target-compatibility");
        assert_eq!(self.after.stage, "target-compatibility");
        assert_eq!(
            self.after.lifecycle_state,
            ApplicationLifecycleState::Unavailable as i32
        );
        assert_eq!(self.calls_after_change, self.calls_before_change);
    }

    /// Asserts the deployment retained and launched the original device set, then failed the
    /// retained-target recheck without recomputing or recreating its group.
    fn assert_incompatible(&self) {
        assert_eq!(self.retained_devices, vec!["nvidia.com/gpu=0".to_owned()]);
        assert_eq!(self.calls_before_change.len(), 1);
        let (call, launch) = &self.calls_before_change[0];
        assert_eq!(*call, "create");
        assert_eq!(
            launch.execution_target.accelerator_device_ids,
            vec!["nvidia.com/gpu=0".to_owned()]
        );
        self.assert_target_incompatible();
    }
}

/// The application execution target bound to the Docker application runtime with `devices`.
fn docker_application_target(devices: Vec<String>) -> ExecutionTarget {
    let mut target = application_execution_target();
    target.accelerator_device_ids = devices;
    target.runtime_binding = Some(execution_target::RuntimeBinding::ApplicationRuntime(
        ApplicationRuntimeBinding {
            adapter: Some(application_runtime_binding::Adapter::DockerRuntime(
                DockerApplicationRuntimeAdapter {},
            )),
        },
    ));
    target
}

#[tokio::test]
async fn deployment_creation_resolves_parameter_overrides_over_defaults() {
    let directory = test_directory();
    let (controller, desired) = parameter_test_controller(&directory);

    let deployment = controller
        .create_deployment(parameter_request(vec![string_parameter(
            "mode", "highvram",
        )]))
        .await
        .expect("deployment creation");

    let expected_values = vec![
        string_parameter("mode", "highvram"),
        ParameterValue {
            parameter_id: "workers".to_owned(),
            value: Some(parameter_value::Value::UnsignedInteger(2)),
        },
    ];
    assert_eq!(
        deployment
            .snapshot
            .as_ref()
            .expect("snapshot")
            .parameter_values,
        expected_values
    );
    let declaration = desired
        .snapshot()
        .expect("persisted declaration")
        .deployments[0]
        .clone();
    assert_eq!(
        declaration
            .snapshot
            .as_ref()
            .expect("snapshot")
            .parameter_values,
        expected_values
    );
    assert_eq!(
        declaration
            .configuration_attachments
            .iter()
            .map(|attachment| (
                attachment.environment_variable.as_str(),
                attachment.value.as_str()
            ))
            .collect::<Vec<_>>(),
        [("MODE", "highvram"), ("WORKERS", "2")]
    );
    let launch = workload_group_launch(
        &declaration,
        Ipv4Addr::LOCALHOST,
        test_inference_route_resolver().as_ref(),
    )
    .expect("workload launch");
    assert_eq!(
        launch.units[&WorkloadUnitName("web".to_owned())].environment,
        BTreeMap::from([
            ("MODE".to_owned(), "highvram".to_owned()),
            ("WORKERS".to_owned(), "2".to_owned()),
        ])
    );
    drop(controller);

    let path = directory.join("site.yaml");
    let mut persisted_site = site();
    persisted_site.application_deployments = vec![declaration.clone()];
    fs::write(
        &path,
        dotcore_proto::yaml::to_string(&persisted_site).expect("site YAML"),
    )
    .expect("write site YAML");
    let reloaded = ApplicationDeclarationRepository::snapshot(
        &SiteRepository::load(&path, crate::test_support::site_declaration_rules())
            .expect("reloaded site repository"),
    )
    .expect("validated declarations after reload");
    assert_eq!(reloaded.deployments, [declaration]);
    fs::remove_dir_all(directory).expect("test directory cleanup");
}

#[tokio::test]
async fn invalid_parameter_values_reject_creation_without_mutation() {
    let directory = test_directory();
    let (controller, desired) = parameter_test_controller(&directory);
    let cases = [
        (
            vec![string_parameter("other", "auto")],
            "Application parameter \"other\" is not declared by the application",
        ),
        (
            vec![
                string_parameter("mode", "auto"),
                string_parameter("mode", "auto"),
            ],
            "Application parameter \"mode\" is supplied more than once",
        ),
        (
            vec![ParameterValue {
                parameter_id: "mode".to_owned(),
                value: None,
            }],
            "Application parameter \"mode\" has no value",
        ),
        (
            vec![string_parameter("mode", "lowvram")],
            "Application parameter \"mode\" has an invalid value: \"lowvram\" is not one of [\"auto\", \"highvram\"]",
        ),
        (
            vec![ParameterValue {
                parameter_id: "workers".to_owned(),
                value: Some(parameter_value::Value::UnsignedInteger(9)),
            }],
            "Application parameter \"workers\" has an invalid value: 9 is outside 1..=8",
        ),
        (
            vec![string_parameter("workers", "2")],
            "Application parameter \"workers\" has an invalid value: value type does not match the parameter type",
        ),
    ];

    for (values, expected) in cases {
        let error = controller
            .create_deployment(parameter_request(values))
            .await
            .expect_err("invalid parameter values");
        assert!(matches!(
            error,
            ApplicationLifecycleError::InvalidParameter { .. }
        ));
        assert_eq!(error.to_string(), expected);
    }
    assert!(
        desired
            .snapshot()
            .expect("declaration snapshot")
            .deployments
            .is_empty()
    );
    drop(controller);
    std::fs::remove_dir_all(directory).expect("test directory cleanup");
}

fn parameter_test_controller(
    directory: &Path,
) -> (
    TestController<TestApplicationDeclarationRepository, InMemoryCurrentState>,
    TestApplicationDeclarationRepository,
) {
    let desired = TestApplicationDeclarationRepository::open(directory, "site-one")
        .expect("declaration repository");
    let current =
        TestCurrentApplicationRepository::open(directory, "site-one").expect("current repository");
    let mut runtimes = crate::dispatch::application::ApplicationRuntimeMapBuilder::new();
    runtimes
        .insert(
            "application-target-one",
            RestartRuntime {
                calls: Arc::new(Mutex::new(Vec::new())),
            },
        )
        .expect("Runtime adapter");
    let controller = TestController::new(
        Arc::new(
            CatalogBuilder::new()
                .application("test", application_with_parameters())
                .build()
                .expect("Catalog"),
        ),
        desired.clone(),
        current,
        runtimes.build(),
        ApplicationControllerConfiguration {
            target_capabilities: test_target_capability_source(),
            configuration_sources: ApplicationConfigurationSources::default(),
            inference_route_resolver: test_inference_route_resolver(),
        },
    )
    .expect("Apps controller");
    (controller, desired)
}

fn parameter_request(parameter_values: Vec<ParameterValue>) -> CreateApplicationDeploymentRequest {
    CreateApplicationDeploymentRequest {
        application_id: "open-webui".to_owned(),
        placement: Some(exact_placement()),
        inference_route_id: None,
        expected_inference_route_generation: 0,
        application_deployment_name: None,
        parameter_values,
    }
}

fn string_parameter(id: &str, value: &str) -> ParameterValue {
    ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(parameter_value::Value::String(value.to_owned())),
    }
}
