use std::{
    future::ready,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime},
};

use dotcore_artifact::{
    ArtifactManager, OciImagePreparer, OciPlatform, OciPreparationFuture, OciPreparationRequest,
    PreparedModelArtifacts,
};
use dotcore_catalog::{Catalog, CatalogModelId, OciArtifactPolicy};
use dotcore_domain::SiteNodeId;
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::{
    catalog::v1::{
        ModelOptionParameterValue, ModelOptionValues, Selector,
        model_option_parameter_value::Value as ModelOptionValue,
    },
    model::v1::{
        LocalModelDeploymentConfiguration, ModelDeploymentSnapshot, model_deployment_snapshot,
    },
    observation::v1::AcceleratorMemoryArchitecture,
};
use dotcore_runtime::{
    ExecutionTarget, ObservedWorkload, RuntimeBackend, RuntimeBackendError, RuntimeBackendId,
    RuntimeFuture, WorkloadId, WorkloadLaunch, WorkloadObservation, WorkloadStorageKey,
    WorkloadStorageKind,
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::hardware::selection::{select_target_capabilities, selector_matches};
use crate::model::{resolution::resolve_local_launch, resolved::ResolvedModelLaunch};

#[tokio::test]
async fn facade_launches_exact_materialized_workload_and_reports_private_facts() {
    let catalog = development_catalog();
    let cache_root = temporary_cache_root();
    let oci = Arc::new(RecordingOciPreparer::default());
    let artifacts =
        ArtifactManager::new(cache_root.clone(), Some(oci.clone())).expect("Artifact manager");
    let configuration = selected_configuration(&catalog);
    let expected_artifacts = seed_selected_artifacts(&catalog, &artifacts, &configuration);
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let facade = DirectWorkloadFacade::new(
        catalog,
        artifacts,
        runtime.clone(),
        DevelopmentRuntimeNamespace::derive("experiment-one")
            .expect("development runtime namespace"),
    );

    let facts = facade
        .launch(
            DirectWorkloadLaunchRequest {
                handle_path: cache_root.join("ownership.json"),
                owner_id: "development-run-one".to_owned(),
                model_id: CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
                profile_id: FIXTURE_PROFILE_ID.to_owned(),
                configuration,
                execution_target: ExecutionTarget {
                    managed_node_id: SiteNodeId::for_test("development-node"),
                    accelerator_device_ids: vec!["0".to_owned()],
                },
            },
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("direct development launch");

    let launches = runtime.launches();
    assert_eq!(launches.len(), 1);
    let launch = launches.into_iter().next().expect("one runtime launch");
    assert_eq!(
        launch.spec.required_platform,
        dotcore_domain::ExecutionPlatform::linux_amd64()
    );
    assert_eq!(facts.format_version, 2);
    assert_eq!(facts.model_id, CatalogModelId(FIXTURE_MODEL_ID.to_owned()));
    assert_eq!(facts.profile_id, FIXTURE_PROFILE_ID);
    assert_eq!(facts.configuration_id, "fixture-configuration");
    assert_eq!(facts.model_artifact_id, "fixture-artifact");
    assert_eq!(
        facts.auxiliary_artifact_ids,
        ["vision-projector", "mtp-draft"]
    );
    assert_eq!(facts.engine_release_id, "fixture-engine");
    assert_eq!(facts.engine_artifact_id, "fixture-linux");
    assert_eq!(facts.engine_version, "1");
    assert_eq!(facts.desired_launch_plan.format_version, 4);
    assert_eq!(
        facts.runtime_backend_id,
        RuntimeBackendId("runtime/docker".to_owned())
    );
    assert_eq!(facts.workload_id, launch.correlation_workload_id());
    assert_eq!(
        facts.workload_id.0,
        "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d_2886705573666796869"
    );
    assert_eq!(
        facts.correlation,
        WorkloadCorrelation {
            owner_id: "runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d".to_owned(),
            reconcile_generation: 2_886_705_573_666_796_869,
        }
    );
    assert_eq!(facts.correlation, launch.correlation);
    assert_eq!(facts.runtime_process_id, "sha256:development-container");
    assert_eq!(facts.process_id, 4242);
    assert_eq!(facts.materialized_mounts, launch.spec.artifacts);
    assert_eq!(
        facts.materialized_argv,
        [
            "/app/llama-server",
            "--model",
            "/opt/dotcore/models/model.gguf",
            "--alias",
            FIXTURE_MODEL_ID,
            "--host",
            "0.0.0.0",
            "--port",
            "8080",
            "--ctx-size",
            "32768",
            "--n-gpu-layers",
            "99",
            "--flash-attn",
            "on",
            "--jinja",
            "--cache-ram",
            "2048",
            "-np",
            "4",
            "--kv-unified",
            "--mmproj",
            "/opt/dotcore/models/projector.gguf",
            "--image-min-tokens",
            "280",
            "--image-max-tokens",
            "280",
            "--spec-type",
            "draft-mtp",
            "--model-draft",
            "/opt/dotcore/models/speculative-draft.gguf",
            "--spec-draft-n-max",
            "2",
            "--seed",
            "7",
        ]
    );
    assert_eq!(
        facts.materialized_mounts,
        [
            dotcore_runtime::WorkloadArtifactBinding {
                name: "model".to_owned(),
                source: expected_artifacts.path("model").clone(),
                destination: "/opt/dotcore/models/model.gguf".to_owned(),
                read_only: true,
            },
            dotcore_runtime::WorkloadArtifactBinding {
                name: "speculative-draft".to_owned(),
                source: expected_artifacts.path("speculative-draft").clone(),
                destination: "/opt/dotcore/models/speculative-draft.gguf".to_owned(),
                read_only: true,
            },
            dotcore_runtime::WorkloadArtifactBinding {
                name: "vision-projector".to_owned(),
                source: expected_artifacts.path("vision-projector").clone(),
                destination: "/opt/dotcore/models/projector.gguf".to_owned(),
                read_only: true,
            },
        ]
    );
    assert_eq!(
        facts.loopback_endpoints,
        [DevelopmentLoopbackEndpoint {
            port_name: "http".to_owned(),
            host: "127.0.0.1".to_owned(),
            host_port: 49152,
        }]
    );
    assert_eq!(
        facts.materialized_argv[facts.materialized_argv.len() - 2..],
        ["--seed", "7"]
    );
    let handle = OwnershipHandleFile::open(&cache_root.join("ownership.json")).expect("handle");
    assert_eq!(
        handle.handle().launch_inputs.custom_argument_tokens,
        ["--seed", "7"]
    );
    assert_eq!(
            oci.requests(),
            [OciPreparationRequest {
                reference: "registry.dotcore.test/fixture@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
                platform: OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                },
                source: dotcore_artifact::OciPreparationSource::ImmutableRemote,
            }]
        );
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn a_target_naming_every_accelerator_device_is_rejected_before_runtime() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, mut request, root, _) = direct_fixture(runtime.clone(), "every-device", true);
    request.execution_target.accelerator_device_ids = vec!["nvidia.com/gpu=all".to_owned()];

    let outcome = facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await;

    assert!(matches!(
        outcome,
        Err(DirectWorkloadLaunchError::EveryAcceleratorDevice)
    ));
    assert_eq!(runtime.namespace_checks.load(Ordering::SeqCst), 0);
    assert!(runtime.launches().is_empty());
    std::fs::remove_dir_all(root).expect("fixture cleanup");
}

#[tokio::test]
async fn concurrent_launches_in_one_scope_cross_only_one_namespace_preflight() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::with_create_gate());
    let (first_facade, first_request, first_root, first_handle) =
        direct_fixture(runtime.clone(), "concurrent-first", true);
    let (second_facade, second_request, second_root, _) =
        direct_fixture(runtime.clone(), "concurrent-second", true);
    let gate = runtime.create_gate();

    let mut first = tokio::spawn(async move {
        first_facade
            .launch(
                first_request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
    });
    tokio::select! {
        () = gate.entered.notified() => {}
        outcome = &mut first => {
            let detail = match outcome {
                Ok(Ok(_)) => "completed without waiting at the gate".to_owned(),
                Ok(Err(error)) => format!("failed with {error:?}"),
                Err(error) => format!("did not finish: {error}"),
            };
            panic!("the first launch never reached the create gate: it {detail}");
        }
        () = tokio::time::sleep(crate::test_support::DIAGNOSTIC_DEADLOCK_WATCHDOG) => {
            panic!("the first launch never reached the create gate within the deadline");
        }
    }
    let second = tokio::spawn(async move {
        second_facade
            .launch(
                second_request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(runtime.namespace_checks.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create"]
    );

    gate.release.notify_one();
    first.await.expect("first task").expect("first launch");
    assert!(matches!(
        second.await.expect("second task"),
        Err(DirectWorkloadLaunchError::NamespaceNotEmpty)
    ));
    assert_eq!(runtime.namespace_checks.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );

    let (cleanup_facade, _, cleanup_root, _) =
        direct_fixture(runtime.clone(), "concurrent-cleanup", true);
    cleanup_facade
        .teardown(&first_handle)
        .await
        .expect("cleanup");
    std::fs::remove_dir_all(first_root).expect("first cleanup");
    std::fs::remove_dir_all(second_root).expect("second cleanup");
    std::fs::remove_dir_all(cleanup_root).expect("facade cleanup");
}

#[tokio::test]
async fn a_runtime_that_starts_on_create_launches_and_tears_down_with_the_same_custody() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::starting_on_create());
    let (facade, request, _root, handle_path) =
        direct_fixture(runtime.clone(), "starts-on-create", true);

    let facts = facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("direct development launch");

    assert_eq!(
        runtime.mutations(),
        ["create"],
        "a runtime that starts on create is never started"
    );
    assert_eq!(
        facts.materialized_argv,
        [
            "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-llama-cpp/bin/llama-server",
            "--port",
            "49152"
        ],
        "the runtime's own resolved command is reported"
    );
    assert_eq!(facts.process_id, 4242);
    assert_eq!(facts.runtime_process_id, "sha256:development-container");
    let inspection = facade
        .inspect_handle(&handle_path)
        .await
        .expect("inspection");
    assert_eq!(inspection.state, DevelopmentInspectionState::Running);
    assert_eq!(inspection.handle.state, DevelopmentOwnershipState::Running);

    let removed = facade.teardown(&handle_path).await.expect("teardown");

    assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
    assert_eq!(runtime.mutations(), ["create", "stop", "remove"]);
}

#[tokio::test]
async fn a_runtime_that_starts_on_create_must_report_its_workload_running() {
    let runtime = Arc::new(RecordingDevelopmentRuntime {
        starts_on_create: true,
        ..RecordingDevelopmentRuntime::default()
    });
    *runtime.running.lock().expect("running") = false;
    let (facade, request, _root, handle_path) =
        direct_fixture(runtime.clone(), "starts-stopped", true);
    runtime
        .process_override
        .lock()
        .expect("process override")
        .replace(DevelopmentProcessFacts {
            runtime_process_id: "sha256:development-container".to_owned(),
            process_id: None,
            launch_fingerprint: "0".repeat(64),
            command: None,
            loopback_endpoints: Vec::new(),
        });

    let error = facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect_err("created process facts without a running process");

    assert!(
        matches!(error, DirectWorkloadLaunchError::IdentityMismatch),
        "{error}"
    );
    let handle = OwnershipHandleFile::open(&handle_path).expect("handle");
    assert_eq!(handle.handle().state, DevelopmentOwnershipState::Dirty);
}

#[tokio::test]
async fn teardown_validates_exact_facts_proves_absence_and_is_idempotent() {
    let catalog = development_catalog();
    let cache_root = temporary_cache_root();
    let artifacts = ArtifactManager::new(
        cache_root.clone(),
        Some(Arc::new(RecordingOciPreparer::default())),
    )
    .expect("Artifact manager");
    let configuration = selected_configuration(&catalog);
    seed_selected_artifacts(&catalog, &artifacts, &configuration);
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let facade = DirectWorkloadFacade::new(
        catalog,
        artifacts,
        runtime.clone(),
        DevelopmentRuntimeNamespace::derive("experiment-one")
            .expect("development runtime namespace"),
    );
    let handle_path = cache_root.join("ownership.json");
    facade
        .launch(
            DirectWorkloadLaunchRequest {
                handle_path: handle_path.clone(),
                owner_id: "development-run-one".to_owned(),
                model_id: CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
                profile_id: FIXTURE_PROFILE_ID.to_owned(),
                configuration,
                execution_target: ExecutionTarget {
                    managed_node_id: SiteNodeId::for_test("development-node"),
                    accelerator_device_ids: vec!["0".to_owned()],
                },
            },
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");

    let removed = facade.teardown(&handle_path).await.expect("teardown");
    assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
    assert_eq!(
        removed.workload_id.as_deref(),
        Some(
            "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d_2886705573666796869"
        )
    );
    assert_eq!(
        removed.runtime_process_id.as_deref(),
        Some("sha256:development-container")
    );
    assert_eq!(removed.process_id, Some(4242));
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start", "stop", "remove"]
    );
    let inspection = facade.inspect_handle(&handle_path).await.expect("inspect");
    assert_eq!(inspection.state, DevelopmentInspectionState::Removed);
    assert_eq!(inspection.handle, removed);
    assert_eq!(inspection.workload_id, None);
    assert_eq!(inspection.runtime_process_id, None);
    assert_eq!(inspection.process_id, None);

    let repeated = facade
        .teardown(&handle_path)
        .await
        .expect("repeated teardown");
    assert_eq!(repeated, removed);
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start", "stop", "remove"]
    );
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn stopped_workload_retains_exact_log_custody_after_process_exit() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, cache_root, handle_path) =
        direct_fixture(runtime.clone(), "stopped-log-custody", true);
    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");
    *runtime.running.lock().expect("running") = false;

    let (scope, transaction, workload_id) = facade
        .prove_log_custody(&handle_path)
        .await
        .expect("stopped workload log custody");

    assert_eq!(
        workload_id.0,
        "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d_2886705573666796869"
    );
    assert_eq!(transaction.handle().process_id, Some(4242));
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );
    drop(transaction);
    drop(scope);
    let inspection = facade.inspect_handle(&handle_path).await.expect("inspect");
    assert_eq!(inspection.state, DevelopmentInspectionState::Stopped);
    assert_eq!(inspection.handle.state, DevelopmentOwnershipState::Running);
    assert_eq!(
        inspection.workload_id.as_ref().map(|id| id.0.as_str()),
        inspection.handle.workload_id.as_deref()
    );
    assert_eq!(
        inspection.runtime_process_id.as_deref(),
        inspection.handle.runtime_process_id.as_deref()
    );
    assert_eq!(inspection.process_id, None);
    facade.teardown(&handle_path).await.expect("teardown");
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn launch_failures_record_attempts_and_leave_exact_recoverable_custody() {
    for (boundary, expected_dirty_identity, expected_attempts, expected_recovery) in [
        (
            "create",
            (None, None),
            &["create"][..],
            &["create", "remove"][..],
        ),
        (
            "start",
            (
                Some(
                    "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d_2886705573666796869",
                ),
                Some("sha256:development-container"),
            ),
            &["create", "start"][..],
            &["create", "start", "stop", "remove"][..],
        ),
        (
            "inspect",
            (
                Some(
                    "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d_2886705573666796869",
                ),
                None,
            ),
            &["create"][..],
            &["create", "remove"][..],
        ),
    ] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::fail_at(boundary));
        let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), boundary, true);

        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect_err("injected launch failure");
        let dirty = OwnershipHandleFile::open(&handle_path).expect("dirty handle");
        assert_eq!(dirty.handle().state, DevelopmentOwnershipState::Dirty);
        assert_eq!(
            (
                dirty.handle().workload_id.as_deref(),
                dirty.handle().runtime_process_id.as_deref(),
            ),
            expected_dirty_identity
        );
        assert_eq!(dirty.handle().process_id, None);
        assert_eq!(dirty.handle().runtime_backend_id, "runtime/docker");
        assert_eq!(dirty.handle().launch_fingerprint.len(), 64);
        assert_eq!(
            runtime.mutations.lock().expect("mutations").as_slice(),
            expected_attempts
        );
        drop(dirty);

        runtime.clear_failure();
        let removed = facade
            .teardown(&handle_path)
            .await
            .expect("recovered teardown");
        assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
        assert!(!*runtime.present.lock().expect("present"));
        assert_eq!(
            runtime.mutations.lock().expect("mutations").as_slice(),
            expected_recovery
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[tokio::test]
async fn post_rename_handle_sync_failure_is_recoverable_without_runtime_mutation() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) =
        direct_fixture(runtime.clone(), "post-rename-sync", true);
    handle::fail_next_parent_sync();

    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect_err("injected parent sync failure");

    let persisted = OwnershipHandleFile::open(&handle_path).expect("committed handle");
    assert_eq!(
        persisted.handle().state,
        DevelopmentOwnershipState::Prepared
    );
    drop(persisted);
    assert!(runtime.mutations.lock().expect("mutations").is_empty());

    let removed = facade
        .teardown(&handle_path)
        .await
        .expect("recover committed handle");
    assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
    assert!(runtime.mutations.lock().expect("mutations").is_empty());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn preparation_failure_has_no_handle_and_never_mutates_runtime() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), "prepare", false);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    facade
        .launch(request, cancellation, DevelopmentLaunchProgress::default())
        .await
        .expect_err("cancelled preparation");

    assert!(!handle_path.exists());
    assert!(runtime.launches().is_empty());
    assert!(runtime.mutations.lock().expect("mutations").is_empty());
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn stop_and_remove_failures_remain_dirty_non_destructive_and_converge_on_retry() {
    for boundary in ["stop", "remove"] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), boundary, true);
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        *runtime.failure.lock().expect("failure") = Some(boundary);

        facade
            .teardown(&handle_path)
            .await
            .expect_err("injected teardown failure");
        let dirty = OwnershipHandleFile::open(&handle_path).expect("dirty handle");
        assert_eq!(dirty.handle().state, DevelopmentOwnershipState::Dirty);
        assert!(dirty.handle().dirty_reason.is_some());
        drop(dirty);
        let inspection = facade
            .inspect_handle(&handle_path)
            .await
            .expect("inspect dirty");
        assert_eq!(inspection.state, DevelopmentInspectionState::Dirty);

        runtime.clear_failure();
        let removed = facade.teardown(&handle_path).await.expect("retry teardown");
        assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
        assert!(!*runtime.present.lock().expect("present"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[tokio::test]
async fn teardown_removes_the_runner_deployment_storage_and_keeps_engine_artifact_storage() {
    let catalog = development_catalog();
    let cache_root = temporary_cache_root();
    let artifacts = ArtifactManager::new(
        cache_root.clone(),
        Some(Arc::new(RecordingOciPreparer::default())),
    )
    .expect("Artifact manager");
    let configuration = selected_configuration(&catalog);
    seed_selected_artifacts(&catalog, &artifacts, &configuration);
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let facade = DirectWorkloadFacade::new(
        catalog,
        artifacts,
        runtime.clone(),
        DevelopmentRuntimeNamespace::derive("experiment-one")
            .expect("development runtime namespace"),
    );
    let handle_path = cache_root.join("ownership.json");
    facade
        .launch(
            DirectWorkloadLaunchRequest {
                handle_path: handle_path.clone(),
                owner_id: "development-run-one".to_owned(),
                model_id: CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
                profile_id: FIXTURE_PROFILE_ID.to_owned(),
                configuration,
                execution_target: ExecutionTarget {
                    managed_node_id: SiteNodeId::for_test("development-node"),
                    accelerator_device_ids: vec!["0".to_owned()],
                },
            },
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");

    let runner = "runner-5441427ad44c87d0862f074f160f4685e86327a2f8a8b6b572d6ef0d";
    let launches = runtime.launches();
    let attached = launches[0]
        .spec
        .storage
        .iter()
        .map(|storage| (storage.namespace.as_str(), storage.key.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(attached.len(), 2);
    assert_eq!(attached[0], ("deployment", runner));
    assert_eq!(attached[1].0, "engine-artifact");
    assert_eq!(runtime.removed_storage(), []);

    facade.teardown(&handle_path).await.expect("teardown");

    let deployment_key = |kind| WorkloadStorageKey {
        kind,
        namespace: "deployment".to_owned(),
        key: runner.to_owned(),
    };
    let attached_deployment_keys = launches[0]
        .spec
        .storage
        .iter()
        .filter(|storage| storage.namespace == "deployment")
        .map(|storage| storage.storage_key())
        .collect::<Vec<_>>();
    assert_eq!(
        attached_deployment_keys,
        [deployment_key(WorkloadStorageKind::Cache)]
    );
    assert_eq!(
        runtime.removed_storage(),
        attached_deployment_keys,
        "teardown removes exactly the deployment-scoped keys launch attached"
    );

    // A repeated teardown of a removed handle removes nothing more.
    facade
        .teardown(&handle_path)
        .await
        .expect("repeat teardown");
    assert_eq!(runtime.removed_storage(), attached_deployment_keys);
    std::fs::remove_dir_all(cache_root).expect("cache cleanup");
}

#[tokio::test]
async fn every_persisted_launch_and_teardown_boundary_recovers_after_interruption() {
    for (index, state) in [
        DevelopmentOwnershipState::Prepared,
        DevelopmentOwnershipState::Creating,
        DevelopmentOwnershipState::Created,
        DevelopmentOwnershipState::Starting,
        DevelopmentOwnershipState::Started,
        DevelopmentOwnershipState::Running,
        DevelopmentOwnershipState::Stopping,
        DevelopmentOwnershipState::Removing,
        DevelopmentOwnershipState::Dirty,
    ]
    .into_iter()
    .enumerate()
    {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let case = format!("recovery-{index}");
        let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), &case, true);
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        if state == DevelopmentOwnershipState::Prepared {
            *runtime.present.lock().expect("present") = false;
        }
        if matches!(
            state,
            DevelopmentOwnershipState::Created
                | DevelopmentOwnershipState::Starting
                | DevelopmentOwnershipState::Stopping
                | DevelopmentOwnershipState::Removing
        ) {
            *runtime.running.lock().expect("running") = false;
        }
        let mut transaction = OwnershipHandleFile::open(&handle_path).expect("open handle");
        let mut interrupted = transaction.handle().clone();
        interrupted.state = state;
        interrupted.dirty_reason =
            (state == DevelopmentOwnershipState::Dirty).then(|| "interrupted".to_owned());
        if state == DevelopmentOwnershipState::Prepared {
            interrupted.workload_id = None;
            interrupted.runtime_process_id = None;
            interrupted.process_id = None;
        }
        transaction.replace(interrupted).expect("persist boundary");
        drop(transaction);

        let removed = facade
            .teardown(&handle_path)
            .await
            .expect("recover teardown");
        assert_eq!(removed.state, DevelopmentOwnershipState::Removed);
        assert!(!*runtime.present.lock().expect("present"));
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[tokio::test]
async fn live_process_custody_reproves_exact_identity_without_runtime_mutation() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) =
        direct_fixture(runtime.clone(), "live-process-custody", true);
    let launch = facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");

    let (identity, callback_identity) = facade
        .with_live_process(
            &handle_path,
            CancellationToken::new(),
            |identity| async move { identity },
        )
        .await
        .expect("live process custody");

    assert_eq!(callback_identity, identity);
    assert_eq!(identity.runtime_backend_id, launch.runtime_backend_id.0);
    assert_eq!(identity.workload_id, launch.workload_id);
    assert_eq!(identity.owner_id, launch.correlation.owner_id);
    assert_eq!(identity.generation, launch.correlation.reconcile_generation);
    assert_eq!(identity.runtime_process_id, launch.runtime_process_id);
    assert_eq!(identity.process_id, launch.process_id);
    assert_eq!(
        identity.launch_fingerprint,
        runtime.launches()[0].exact_intent_fingerprint()
    );
    assert_eq!(runtime.namespace_checks.load(Ordering::SeqCst), 3);
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );

    facade.teardown(&handle_path).await.expect("cleanup");
    std::fs::remove_dir_all(root).expect("cleanup root");
}

#[tokio::test]
async fn live_process_custody_wait_is_cancellable() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) = direct_fixture(runtime, "live-process-cancel", true);
    let facade = Arc::new(facade);
    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first_facade = facade.clone();
    let first_handle = handle_path.clone();
    let first_entered = entered.clone();
    let first_release = release.clone();
    let first = tokio::spawn(async move {
        first_facade
            .with_live_process(
                &first_handle,
                CancellationToken::new(),
                move |_| async move {
                    first_entered.notify_one();
                    first_release.notified().await;
                },
            )
            .await
    });
    entered.notified().await;
    let cancellation = CancellationToken::new();
    let second_cancellation = cancellation.clone();
    let second_facade = facade.clone();
    let second_handle = handle_path.clone();
    let second = tokio::spawn(async move {
        second_facade
            .with_live_process(&second_handle, second_cancellation, |_| async move {
                panic!("cancelled custody must not invoke the operation");
            })
            .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancellation.cancel();

    let error = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("cancelled custody wait deadline")
        .expect("custody task")
        .expect_err("cancelled custody wait");

    assert!(matches!(
        error,
        DirectWorkloadTeardownError::OwnershipHandle(OwnershipHandleError::Cancelled)
    ));
    release.notify_one();
    first
        .await
        .expect("first custody task")
        .expect("first custody operation");
    facade.teardown(&handle_path).await.expect("cleanup");
    std::fs::remove_dir_all(root).expect("cleanup root");
}

#[tokio::test]
async fn live_process_custody_rejects_stale_or_nonrunning_handles_before_callback() {
    for case in ["stopped", "removed", "stale"] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let (facade, request, root, handle_path) = direct_fixture(
            runtime.clone(),
            &format!("live-process-before-{case}"),
            true,
        );
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        match case {
            "stopped" => *runtime.running.lock().expect("running") = false,
            "removed" => {
                facade
                    .teardown(&handle_path)
                    .await
                    .expect("remove workload");
            }
            "stale" => {
                let mut transaction = OwnershipHandleFile::open(&handle_path).expect("open handle");
                let mut stale = transaction.handle().clone();
                stale.generation += 1;
                transaction.replace(stale).expect("persist stale handle");
            }
            _ => unreachable!(),
        }
        let callback_count = Arc::new(AtomicUsize::new(0));
        let observed_callback_count = callback_count.clone();

        let error = facade
            .with_live_process(
                &handle_path,
                CancellationToken::new(),
                move |_| async move {
                    observed_callback_count.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await
            .expect_err("unusable workload handle");

        assert!(matches!(error, DirectWorkloadTeardownError::Dirty(_)));
        assert_eq!(callback_count.load(Ordering::SeqCst), 0);
        if case == "stopped" {
            facade.teardown(&handle_path).await.expect("cleanup");
        }
        std::fs::remove_dir_all(root).expect("cleanup root");
    }
}

#[tokio::test]
async fn live_process_custody_rejects_identity_changes_after_callback() {
    for case in [
        "removed",
        "workload",
        "generation",
        "container",
        "fingerprint",
        "process",
    ] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let (facade, request, root, handle_path) = direct_fixture(
            runtime.clone(),
            &format!("live-process-during-{case}"),
            true,
        );
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        let launch_fingerprint = runtime.launches()[0].exact_intent_fingerprint();
        let callback_runtime = runtime.clone();

        let error = facade
            .with_live_process(
                &handle_path,
                CancellationToken::new(),
                move |_| async move {
                    match case {
                        "removed" => *callback_runtime.present.lock().expect("present") = false,
                        "workload" => {
                            callback_runtime
                                .launches
                                .lock()
                                .expect("launches")
                                .last_mut()
                                .expect("launch")
                                .correlation
                                .owner_id = "replacement-owner".to_owned();
                        }
                        "generation" => {
                            callback_runtime
                                .launches
                                .lock()
                                .expect("launches")
                                .last_mut()
                                .expect("launch")
                                .correlation
                                .reconcile_generation += 1;
                        }
                        "container" | "fingerprint" | "process" => {
                            *callback_runtime
                                .process_override
                                .lock()
                                .expect("process override") = Some(DevelopmentProcessFacts {
                                runtime_process_id: if case == "container" {
                                    "replacement-container".to_owned()
                                } else {
                                    "sha256:development-container".to_owned()
                                },
                                process_id: Some(if case == "process" { 5252 } else { 4242 }),
                                launch_fingerprint: if case == "fingerprint" {
                                    "f".repeat(64)
                                } else {
                                    launch_fingerprint
                                },
                                command: None,
                                loopback_endpoints: Vec::new(),
                            });
                        }
                        _ => unreachable!(),
                    }
                },
            )
            .await
            .expect_err("changed identity");

        assert!(matches!(error, DirectWorkloadTeardownError::Dirty(_)));
        assert_eq!(
            runtime.mutations.lock().expect("mutations").as_slice(),
            ["create", "start"]
        );
        std::fs::remove_dir_all(root).expect("cleanup root");
    }
}

#[tokio::test]
async fn inspection_rejects_replaced_or_altered_live_runtime_evidence() {
    for case in ["workload", "container", "fingerprint"] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let (facade, request, root, handle_path) =
            direct_fixture(runtime.clone(), &format!("inspect-{case}"), true);
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        match case {
            "workload" => {
                runtime
                    .launches
                    .lock()
                    .expect("launches")
                    .last_mut()
                    .expect("launch")
                    .correlation
                    .owner_id = "replacement-owner".to_owned()
            }
            "container" => {
                *runtime.process_override.lock().expect("process override") =
                    Some(DevelopmentProcessFacts {
                        runtime_process_id: "replacement-container".to_owned(),
                        process_id: Some(4242),
                        launch_fingerprint: runtime.launches()[0].exact_intent_fingerprint(),
                        command: None,
                        loopback_endpoints: Vec::new(),
                    });
            }
            "fingerprint" => {
                *runtime.process_override.lock().expect("process override") =
                    Some(DevelopmentProcessFacts {
                        runtime_process_id: "sha256:development-container".to_owned(),
                        process_id: Some(4242),
                        launch_fingerprint: "f".repeat(64),
                        command: None,
                        loopback_endpoints: Vec::new(),
                    });
            }
            _ => unreachable!(),
        }

        assert!(matches!(
            facade.inspect_handle(&handle_path).await,
            Err(DirectWorkloadTeardownError::Dirty(_))
        ));
        assert_eq!(
            runtime.mutations.lock().expect("mutations").as_slice(),
            ["create", "start"]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[tokio::test]
async fn sigint_cancellation_never_adopts_a_preexisting_handle_or_workload() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) =
        direct_fixture(runtime.clone(), "preexisting-cancel", true);
    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("preexisting launch");
    let (_, second_request, second_root, _) =
        direct_fixture(runtime.clone(), "preexisting-cancel-second", false);
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let progress = DevelopmentLaunchProgress::default();

    facade
        .launch(second_request, cancellation, progress.clone())
        .await
        .expect_err("cancelled preparation");

    assert!(!progress.ownership_established());
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );
    let inspection = facade.inspect_handle(&handle_path).await.expect("inspect");
    assert_eq!(inspection.state, DevelopmentInspectionState::Running);
    facade.teardown(&handle_path).await.expect("cleanup");
    std::fs::remove_dir_all(root).expect("cleanup root");
    std::fs::remove_dir_all(second_root).expect("second cleanup root");
}

#[tokio::test]
async fn altered_launch_intent_never_mutates_runtime_during_teardown() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) =
        direct_fixture(runtime.clone(), "altered-launch", true);
    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");
    runtime
        .launches
        .lock()
        .expect("launches")
        .first_mut()
        .expect("launch")
        .spec
        .arguments
        .push(dotcore_runtime::WorkloadValue::literal(
            "--foreign-argument",
        ));

    facade
        .teardown(&handle_path)
        .await
        .expect_err("altered launch intent");

    assert!(*runtime.present.lock().expect("present"));
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn foreign_evidence_never_mutates_runtime() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), "altered", true);
    facade
        .launch(
            request,
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("launch");
    let original = runtime
        .launches
        .lock()
        .expect("launches")
        .first()
        .cloned()
        .expect("launch");
    runtime
        .launches
        .lock()
        .expect("launches")
        .first_mut()
        .expect("launch")
        .correlation
        .owner_id = "foreign-owner".to_owned();

    facade
        .teardown(&handle_path)
        .await
        .expect_err("foreign evidence");

    assert!(*runtime.present.lock().expect("present"));
    assert_eq!(
        runtime.mutations.lock().expect("mutations").as_slice(),
        ["create", "start"]
    );
    *runtime.launches.lock().expect("launches") = vec![original];
    runtime.clear_failure();
    facade.teardown(&handle_path).await.expect("exact cleanup");
    std::fs::remove_dir_all(root).expect("cleanup");
}

#[tokio::test]
async fn duplicate_replaced_and_altered_evidence_never_mutates_runtime() {
    for case in ["duplicate", "replaced", "altered"] {
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let (facade, request, root, handle_path) = direct_fixture(runtime.clone(), case, true);
        facade
            .launch(
                request,
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("launch");
        match case {
            "duplicate" => {
                let launch = runtime.launches().into_iter().next().expect("launch");
                runtime
                    .additional_observations
                    .lock()
                    .expect("additional observations")
                    .push(WorkloadObservation::Running(observed_workload(&launch)));
            }
            "replaced" | "altered" => {
                let mut transaction = OwnershipHandleFile::open(&handle_path).expect("handle");
                let mut changed = transaction.handle().clone();
                if case == "replaced" {
                    changed.workload_id = Some("replacement-workload".to_owned());
                } else {
                    changed.runtime_process_id = Some("replacement-container".to_owned());
                }
                transaction.replace(changed).expect("changed evidence");
            }
            _ => unreachable!(),
        }

        facade
            .teardown(&handle_path)
            .await
            .expect_err("unsafe evidence");

        assert!(*runtime.present.lock().expect("present"));
        assert_eq!(
            runtime.mutations.lock().expect("mutations").as_slice(),
            ["create", "start"]
        );
        std::fs::remove_dir_all(root).expect("cleanup");
    }
}

#[test]
fn development_runtime_namespace_is_deterministic_and_cannot_embed_caller_identities() {
    let namespace = DevelopmentRuntimeNamespace::derive("experiment-one")
        .expect("development runtime namespace");

    assert_eq!(
        namespace.runtime_scope,
        "development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978"
    );
    assert_eq!(
        namespace
            .owner_id("site-controller-owner")
            .expect("development owner namespace"),
        "runner-42d75f8c80505a5fb49b8e1f1017efab0de8eff6b8760268c348e2fd"
    );
    assert!(!namespace.runtime_scope.contains("experiment-one"));
    assert!(
        !namespace
            .owner_id("site-controller-owner")
            .expect("development owner namespace")
            .contains("site-controller-owner")
    );
}

#[test]
fn development_runtime_namespace_rejects_unsafe_caller_identities() {
    for value in ["", "site_owner", "Site-owner", "site/owner"] {
        assert!(DevelopmentRuntimeNamespace::derive(value).is_err());
    }
    let namespace = DevelopmentRuntimeNamespace::derive("experiment-one")
        .expect("development runtime namespace");
    for value in ["", "site_owner", "Site-owner", "site/owner"] {
        assert!(namespace.owner_id(value).is_err());
    }
}

#[test]
fn direct_targets_match_device_model_classes_through_their_selected_devices() {
    let description =
        MachineDescription::simulated(&["gb10".to_owned()], "arm64").expect("simulated machine");
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let gb10_class = Selector {
        accelerators: vec!["nvidia".to_owned()],
        memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
        device_models: vec!["NVIDIA GB10".to_owned()],
        ..Default::default()
    };
    let target = ExecutionTarget {
        managed_node_id: SiteNodeId::for_test("development-node"),
        accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
    };

    let (site_target, inventory) = direct_target_capabilities(
        &target,
        model_runtime_binding::Adapter::DockerRuntime(Default::default()),
        description.platform().clone(),
        NodeAcceleratorObservation::from_hardware_snapshot("development-node", &snapshot, false),
        Timing::test().node_control.capability_max_age,
        real_time::wall_now(),
    );
    let selected =
        select_target_capabilities(&site_target, &inventory).expect("target capabilities");

    // The selected GPU's model reaches the target, so the device model class matches it.
    assert_eq!(selected.device_models, ["NVIDIA GB10"]);
    assert!(selector_matches(&gb10_class, &selected));
}

#[tokio::test]
async fn launch_preflight_rejects_unknown_and_wrongly_typed_model_options() {
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let (facade, mut request, _, _) = direct_fixture(runtime.clone(), "option-preflight", false);

    request.configuration.option_overrides = vec![ModelOptionValues {
        option_id: "vision".to_owned(),
        values: vec![ModelOptionParameterValue {
            parameter_id: "unknown".to_owned(),
            value: Some(ModelOptionValue::Boolean(false)),
        }],
    }];
    assert_eq!(
        facade
            .validate_local_launch_configuration(
                &request.model_id,
                &request.profile_id,
                &request.configuration,
                &request.execution_target,
            )
            .await
            .expect_err("unknown option parameter must reject")
            .to_string(),
        "could not resolve direct development launch: invalid model option values: deployment has invalid overrides for option \"vision\""
    );

    request.configuration.option_overrides[0].values[0] = ModelOptionParameterValue {
        parameter_id: "enabled".to_owned(),
        value: Some(ModelOptionValue::UnsignedInteger(1)),
    };
    assert_eq!(
        facade
            .validate_local_launch_configuration(
                &request.model_id,
                &request.profile_id,
                &request.configuration,
                &request.execution_target,
            )
            .await
            .expect_err("wrongly typed option parameter must reject")
            .to_string(),
        "could not resolve direct development launch: invalid model option values: deployment has invalid overrides for option \"vision\""
    );
    assert!(runtime.launches().is_empty());
}

fn direct_fixture(
    runtime: Arc<RecordingDevelopmentRuntime>,
    case: &str,
    seed_artifacts: bool,
) -> (
    DirectWorkloadFacade,
    DirectWorkloadLaunchRequest,
    PathBuf,
    PathBuf,
) {
    let catalog = development_catalog();
    let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-direct-workload-{case}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&root).expect("fixture root");
    let artifacts = ArtifactManager::new(
        root.clone(),
        Some(Arc::new(RecordingOciPreparer::default())),
    )
    .expect("Artifact manager");
    let configuration = selected_configuration(&catalog);
    if seed_artifacts {
        seed_selected_artifacts(&catalog, &artifacts, &configuration);
    }
    let facade = DirectWorkloadFacade::new(
        catalog,
        artifacts,
        runtime,
        DevelopmentRuntimeNamespace::derive("experiment-one")
            .expect("development runtime namespace"),
    );
    let handle_path = root.join("ownership.json");
    let request = DirectWorkloadLaunchRequest {
        handle_path: handle_path.clone(),
        owner_id: "development-run-one".to_owned(),
        model_id: CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
        profile_id: FIXTURE_PROFILE_ID.to_owned(),
        configuration,
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("development-node"),
            accelerator_device_ids: vec!["0".to_owned()],
        },
    };
    (facade, request, root, handle_path)
}

#[tokio::test]
async fn facade_pins_a_tag_only_engine_image_and_prepares_and_launches_by_its_image_id() {
    const IMAGE_ID: &str =
        "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    let reference = crate::test_support::TAG_ONLY_ENGINE_REFERENCE;
    let catalog = || {
        crate::test_support::tag_only_engine_catalog()
            .with_test_model_artifact_digests_omitted("fixture-model")
    };
    let configuration = crate::test_support::local_model_configuration();
    let request = |cache_root: &std::path::Path| DirectWorkloadLaunchRequest {
        handle_path: cache_root.join("ownership.json"),
        owner_id: "development-run-one".to_owned(),
        model_id: CatalogModelId("fixture-model".to_owned()),
        profile_id: "fixture-profile".to_owned(),
        configuration: configuration.clone(),
        execution_target: ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("development-node"),
            accelerator_device_ids: vec!["0".to_owned()],
        },
    };

    let cache_root = crate::test_support::canonical_tempdir().expect("cache root");
    let oci = Arc::new(RecordingOciPreparer::default());
    oci.local_images
        .lock()
        .expect("local images")
        .insert(reference.to_owned(), IMAGE_ID.to_owned());
    let artifacts = ArtifactManager::new(cache_root.path().to_path_buf(), Some(oci.clone()))
        .expect("Artifact manager");
    let (target, inventory) = crate::test_support::execution_target_capabilities(
        dotcore_domain::ExecutionPlatform::linux_amd64(),
    );
    let pinned = resolve_local_launch(
        &catalog(),
        &CatalogModelId("fixture-model".to_owned()),
        "fixture-profile",
        &configuration,
        &target,
        &inventory,
        Some(&crate::model::resolution::EngineLaunchIdentity {
            reference: reference.to_owned(),
            image_id: IMAGE_ID.to_owned(),
        }),
        crate::model::resolution::EngineSecrets::Generate(&crate::RandomSecretGenerator),
    )
    .expect("pinned launch");
    let resolved = ResolvedModelLaunch::from_local_snapshot(&pinned).expect("resolved model");
    for path in artifacts
        .expected_model_paths(&resolved.artifacts)
        .expect("expected artifact paths")
        .artifacts
        .values()
    {
        std::fs::create_dir_all(path.parent().expect("artifact parent")).expect("artifact parent");
        std::fs::write(path, b"development artifact").expect("cached artifact");
    }
    let runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let facade = DirectWorkloadFacade::new(
        catalog(),
        artifacts,
        runtime.clone(),
        DevelopmentRuntimeNamespace::derive("experiment-local-image")
            .expect("development runtime namespace"),
    );

    facade
        .launch(
            request(cache_root.path()),
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect("direct development launch");

    assert_eq!(
        oci.requests(),
        [OciPreparationRequest {
            reference: IMAGE_ID.to_owned(),
            platform: OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            },
            source: dotcore_artifact::OciPreparationSource::LocalImageId,
        }]
    );
    let launches = runtime.launches();
    assert_eq!(launches.len(), 1);
    assert_eq!(launches[0].spec.executable.artifact.reference(), IMAGE_ID);

    let unbuilt_cache_root = crate::test_support::canonical_tempdir().expect("cache root");
    let unbuilt_runtime = Arc::new(RecordingDevelopmentRuntime::default());
    let unbuilt = DirectWorkloadFacade::new(
        catalog(),
        ArtifactManager::new(
            unbuilt_cache_root.path().to_path_buf(),
            Some(Arc::new(RecordingOciPreparer::default())),
        )
        .expect("Artifact manager"),
        unbuilt_runtime.clone(),
        DevelopmentRuntimeNamespace::derive("experiment-unbuilt-image")
            .expect("development runtime namespace"),
    );

    let error = unbuilt
        .launch(
            request(unbuilt_cache_root.path()),
            CancellationToken::new(),
            DevelopmentLaunchProgress::default(),
        )
        .await
        .expect_err("tag names no local image");

    assert_eq!(
        error.to_string(),
        "could not prepare direct development engine artifact: tagged OCI image is not available in the local image store"
    );
    assert_eq!(unbuilt_runtime.launches().len(), 0);
}

#[tokio::test]
async fn facade_pins_prepares_and_launches_a_docker_delivered_engine_of_its_own_system() {
    let (system, store_path) = crate::test_support::nix::DOCKER_IMAGE_STORE_PATHS[0];
    for flake in [
        dotcore_domain::test_support::nix::locked_flake(),
        "self".to_owned(),
    ] {
        let catalog = || {
            crate::test_support::nix::docker_delivered_engine_catalog(&flake)
                .with_test_model_artifact_digests_omitted("fixture-model")
        };
        let configuration = crate::test_support::local_model_configuration();
        let root = crate::test_support::canonical_tempdir().expect("fixture root");
        let images = dotcore_artifact::DockerImageFixture::new(root.path());
        images.add_image(store_path, "amd64");
        let (development_source, evaluator) =
            dotcore_artifact::FixedNixEvaluator::development_source(store_path);
        let artifacts = images.manager(system).with_nix_sources(development_source);
        let (target, inventory) = crate::test_support::execution_target_capabilities(
            dotcore_domain::ExecutionPlatform::linux_amd64(),
        );
        let mut pinned = resolve_local_launch(
            &catalog(),
            &CatalogModelId("fixture-model".to_owned()),
            "fixture-profile",
            &configuration,
            &target,
            &inventory,
            None,
            crate::model::resolution::EngineSecrets::Generate(&crate::RandomSecretGenerator),
        )
        .expect("resolved launch");
        pinned
            .engine_artifact
            .as_mut()
            .expect("engine artifact")
            .launch_identity = store_path.to_owned();
        let resolved = ResolvedModelLaunch::from_local_snapshot(&pinned).expect("resolved model");
        for path in artifacts
            .expected_model_paths(&resolved.artifacts)
            .expect("expected artifact paths")
            .artifacts
            .values()
        {
            std::fs::create_dir_all(path.parent().expect("artifact parent"))
                .expect("artifact parent");
            std::fs::write(path, b"development artifact").expect("cached artifact");
        }
        let runtime = Arc::new(RecordingDevelopmentRuntime::default());
        let facade = DirectWorkloadFacade::new(
            catalog(),
            artifacts,
            runtime.clone(),
            DevelopmentRuntimeNamespace::derive("experiment-docker-delivered")
                .expect("development runtime namespace"),
        );

        facade
            .launch(
                DirectWorkloadLaunchRequest {
                    handle_path: root.path().join("ownership.json"),
                    owner_id: "development-run-one".to_owned(),
                    model_id: CatalogModelId("fixture-model".to_owned()),
                    profile_id: "fixture-profile".to_owned(),
                    configuration: configuration.clone(),
                    execution_target: ExecutionTarget {
                        managed_node_id: SiteNodeId::for_test("development-node"),
                        accelerator_device_ids: vec!["0".to_owned()],
                    },
                },
                CancellationToken::new(),
                DevelopmentLaunchProgress::default(),
            )
            .await
            .expect("direct development launch");

        let launches = runtime.launches();
        assert_eq!(launches.len(), 1, "{flake}");
        assert_eq!(
            launches[0].spec.executable.artifact.reference(),
            dotcore_domain::nix::docker_launch_reference(store_path).expect("reference"),
            "{flake}"
        );
        assert_eq!(images.loader_runs(store_path), 1, "{flake}");
        assert_eq!(
            evaluator.evaluations(),
            usize::from(flake == "self"),
            "{flake}"
        );
    }
}

/// Model and profile of the development fixture catalog: a llama.cpp model whose profile enables
/// vision and keeps the MTP draft on, so one launch materializes every mount and option.
const FIXTURE_MODEL_ID: &str = "fixture-vision-model";
const FIXTURE_PROFILE_ID: &str = "fixture-profile";

fn development_catalog() -> Catalog {
    Catalog::load_with_policy(
        &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/development-catalog"),
        OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    )
    .expect("development fixture catalog")
    .with_test_model_artifact_digests_omitted(FIXTURE_MODEL_ID)
}

fn selected_configuration(catalog: &Catalog) -> LocalModelDeploymentConfiguration {
    let (binding, parameters) = catalog
        .local_defaults_for(
            &CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
            FIXTURE_PROFILE_ID,
        )
        .expect("profile defaults");
    LocalModelDeploymentConfiguration {
        model_artifact_binding_id: binding,
        engine_parameter_values: Some(parameters),
        custom_argument_tokens: vec!["--seed".to_owned(), "7".to_owned()],
        option_overrides: Vec::new(),
    }
}

fn seed_selected_artifacts(
    catalog: &Catalog,
    artifacts: &ArtifactManager,
    configuration: &LocalModelDeploymentConfiguration,
) -> PreparedModelArtifacts {
    let (target, inventory) = crate::test_support::execution_target_capabilities(
        dotcore_domain::ExecutionPlatform::linux_amd64(),
    );
    let local = resolve_local_launch(
        catalog,
        &CatalogModelId(FIXTURE_MODEL_ID.to_owned()),
        FIXTURE_PROFILE_ID,
        configuration,
        &target,
        &inventory,
        None,
        crate::model::resolution::EngineSecrets::Generate(&crate::RandomSecretGenerator),
    )
    .expect("resolved launch");
    let resolved = ResolvedModelLaunch::from_snapshot(&ModelDeploymentSnapshot {
        source: Some(model_deployment_snapshot::Source::Local(local)),
    })
    .expect("resolved model");
    let expected = artifacts
        .expected_model_paths(&resolved.artifacts)
        .expect("expected artifact paths");
    for path in expected.artifacts.values() {
        std::fs::create_dir_all(path.parent().expect("artifact parent")).expect("artifact parent");
        std::fs::write(path, b"development artifact").expect("cached artifact");
    }
    expected
}

fn temporary_cache_root() -> PathBuf {
    dotcore_domain::test_support::canonical_temp_dir().join(format!(
        "dotcore-direct-workload-{}-{}",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ))
}

#[derive(Default)]
struct RecordingOciPreparer {
    requests: Mutex<Vec<OciPreparationRequest>>,
    local_images: Mutex<std::collections::BTreeMap<String, String>>,
}

impl RecordingOciPreparer {
    fn requests(&self) -> Vec<OciPreparationRequest> {
        self.requests.lock().expect("OCI requests").clone()
    }
}

impl OciImagePreparer for RecordingOciPreparer {
    fn prepare(
        &self,
        request: OciPreparationRequest,
        _cancellation: CancellationToken,
        _: dotcore_artifact::StepOutput,
    ) -> OciPreparationFuture {
        self.requests.lock().expect("OCI requests").push(request);
        Box::pin(ready(Ok(())))
    }

    fn as_local_image_identity_resolver(
        &self,
    ) -> Option<&dyn dotcore_artifact::LocalImageIdentityResolver> {
        Some(self)
    }
}

impl dotcore_artifact::LocalImageIdentityResolver for RecordingOciPreparer {
    fn resolve_local_image_identity(
        &self,
        reference: String,
    ) -> dotcore_artifact::LocalImageIdentityResolutionFuture {
        let identity = self
            .local_images
            .lock()
            .expect("local images")
            .get(&reference)
            .cloned()
            .ok_or(dotcore_artifact::LocalImageIdentityResolutionError::LocalImageMissing);
        Box::pin(ready(identity))
    }
}

#[derive(Default)]
struct CreateGate {
    entered: Notify,
    release: Notify,
}

#[derive(Default)]
struct RecordingDevelopmentRuntime {
    launches: Mutex<Vec<WorkloadLaunch>>,
    running: Mutex<bool>,
    present: Mutex<bool>,
    mutations: Mutex<Vec<&'static str>>,
    failure: Mutex<Option<&'static str>>,
    additional_observations: Mutex<Vec<WorkloadObservation>>,
    process_override: Mutex<Option<DevelopmentProcessFacts>>,
    create_gate: Mutex<Option<Arc<CreateGate>>>,
    namespace_checks: AtomicUsize,
    removed_storage: Mutex<Vec<WorkloadStorageKey>>,
    /// Whether creating also starts the workload and reports its command, as a native runtime
    /// does.
    starts_on_create: bool,
}

impl RecordingDevelopmentRuntime {
    fn launches(&self) -> Vec<WorkloadLaunch> {
        self.launches.lock().expect("Runtime launches").clone()
    }

    fn starting_on_create() -> Self {
        Self {
            starts_on_create: true,
            ..Self::default()
        }
    }

    fn removed_storage(&self) -> Vec<WorkloadStorageKey> {
        self.removed_storage
            .lock()
            .expect("removed storage")
            .clone()
    }

    fn mutations(&self) -> Vec<&'static str> {
        self.mutations.lock().expect("mutations").clone()
    }

    fn fail_at(boundary: &'static str) -> Self {
        Self {
            failure: Mutex::new(Some(boundary)),
            ..Self::default()
        }
    }

    fn with_create_gate() -> Self {
        Self {
            create_gate: Mutex::new(Some(Arc::new(CreateGate::default()))),
            ..Self::default()
        }
    }

    fn create_gate(&self) -> Arc<CreateGate> {
        self.create_gate
            .lock()
            .expect("create gate")
            .clone()
            .expect("configured create gate")
    }

    fn clear_failure(&self) {
        self.failure.lock().expect("failure").take();
    }

    fn injected_failure(&self, boundary: &str) -> Option<RuntimeBackendError> {
        (*self.failure.lock().expect("failure") == Some(boundary))
            .then(|| RuntimeBackendError::invalid_launch(format!("injected {boundary} failure")))
    }
}

impl RuntimeBackend for RecordingDevelopmentRuntime {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId("runtime/docker".to_owned())
    }

    fn remove_storage<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        if let Some(error) = self.injected_failure("remove-storage") {
            return Box::pin(ready(Err(error)));
        }
        self.removed_storage
            .lock()
            .expect("removed storage")
            .push(storage.clone());
        Box::pin(ready(Ok(())))
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        if let Some(error) = self.injected_failure("discover") {
            return Box::pin(ready(Err(error)));
        }
        let present = *self.present.lock().expect("present");
        let running = *self.running.lock().expect("running");
        let observation = self
            .launches
            .lock()
            .expect("Runtime launches")
            .last()
            .map(observed_workload)
            .map(|observed| {
                if running {
                    WorkloadObservation::Running(observed)
                } else {
                    WorkloadObservation::stopped(observed)
                }
            });
        let mut observations = if present {
            vec![observation.expect("present workload has launch")]
        } else {
            Vec::new()
        };
        observations.extend(
            self.additional_observations
                .lock()
                .expect("additional observations")
                .clone(),
        );
        Box::pin(ready(Ok(observations)))
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.launches
            .lock()
            .expect("Runtime launches")
            .push(workload.clone());
        let observed = observed_workload(workload);
        Box::pin(ready(Ok(WorkloadObservation::Running(observed))))
    }

    fn start_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        self.mutations.lock().expect("mutations").push("start");
        *self.running.lock().expect("running") = true;
        if let Some(error) = self.injected_failure("start") {
            return Box::pin(ready(Err(error)));
        }
        let launch = self
            .launches
            .lock()
            .expect("Runtime launches")
            .last()
            .cloned()
            .expect("created launch");
        Box::pin(ready(Ok(WorkloadObservation::Running(observed_workload(
            &launch,
        )))))
    }

    fn inspect_workload<'a>(
        &'a self,
        _workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        let running = *self.running.lock().expect("running");
        let launch = self
            .launches
            .lock()
            .expect("Runtime launches")
            .last()
            .cloned()
            .expect("created launch");
        let observed = observed_workload(&launch);
        Box::pin(ready(Ok(if running {
            WorkloadObservation::Running(observed)
        } else {
            WorkloadObservation::stopped(observed)
        })))
    }

    fn stop_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.mutations.lock().expect("mutations").push("stop");
        if let Some(error) = self.injected_failure("stop") {
            return Box::pin(ready(Err(error)));
        }
        *self.running.lock().expect("running") = false;
        Box::pin(ready(Ok(())))
    }

    fn remove_workload<'a>(&'a self, _workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        self.mutations.lock().expect("mutations").push("remove");
        if let Some(error) = self.injected_failure("remove") {
            return Box::pin(ready(Err(error)));
        }
        *self.present.lock().expect("present") = false;
        Box::pin(ready(Ok(())))
    }
}

impl DevelopmentRuntime for RecordingDevelopmentRuntime {
    fn starts_on_create(&self) -> bool {
        self.starts_on_create
    }

    fn namespace_evidence_count<'a>(&'a self) -> RuntimeFuture<'a, usize> {
        self.namespace_checks.fetch_add(1, Ordering::SeqCst);
        let owned = usize::from(*self.present.lock().expect("present"));
        let additional = self
            .additional_observations
            .lock()
            .expect("additional observations")
            .len();
        Box::pin(ready(Ok(owned + additional)))
    }

    fn create<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(async move {
            self.mutations.lock().expect("mutations").push("create");
            let gate = self.create_gate.lock().expect("create gate").clone();
            if let Some(gate) = gate {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
            self.launches
                .lock()
                .expect("Runtime launches")
                .push(workload.clone());
            *self.running.lock().expect("running") = self.starts_on_create;
            *self.present.lock().expect("present") = true;
            if let Some(error) = self.injected_failure("create") {
                return Err(error);
            }
            Ok(if self.starts_on_create {
                WorkloadObservation::Running(observed_workload(workload))
            } else {
                WorkloadObservation::stopped(observed_workload(workload))
            })
        })
    }

    fn inspect_process<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, DevelopmentProcessFacts> {
        if let Some(error) = self.injected_failure("inspect") {
            return Box::pin(ready(Err(error)));
        }
        if let Some(process) = self
            .process_override
            .lock()
            .expect("process override")
            .clone()
        {
            return Box::pin(ready(Ok(process)));
        }
        assert!(workload_id.0.starts_with("dotcore_"));
        let running = *self.running.lock().expect("running");
        let launch_fingerprint = self
            .launches
            .lock()
            .expect("Runtime launches")
            .last()
            .expect("created launch")
            .exact_intent_fingerprint();
        Box::pin(ready(Ok(DevelopmentProcessFacts {
            runtime_process_id: "sha256:development-container".to_owned(),
            process_id: running.then_some(4242),
            launch_fingerprint,
            command: self.starts_on_create.then(|| {
                vec![
                    "/nix/store/0123456789abcdfghijklmnpqrsvwxyz-llama-cpp/bin/llama-server"
                        .to_owned(),
                    "--port".to_owned(),
                    "49152".to_owned(),
                ]
            }),
            loopback_endpoints: if running {
                vec![DevelopmentLoopbackEndpoint {
                    port_name: "http".to_owned(),
                    host: "127.0.0.1".to_owned(),
                    host_port: 49152,
                }]
            } else {
                Vec::new()
            },
        })))
    }
}

fn observed_workload(workload: &WorkloadLaunch) -> ObservedWorkload {
    ObservedWorkload {
        workload_id: workload.correlation_workload_id(),
        correlation: workload.correlation.clone(),
        backend_id: RuntimeBackendId("runtime/docker".to_owned()),
        loopback_endpoints: vec![dotcore_runtime::LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: 49152,
        }],
        services: vec![dotcore_runtime::WorkloadServiceBinding::loopback(
            workload.correlation_workload_id().0,
            "http",
            workload.correlation.reconcile_generation,
            dotcore_runtime::LoopbackEndpoint {
                port_name: "http".to_owned(),
                host_port: 49152,
            },
        )],
        host_process: None,
    }
}

trait CorrelationWorkloadId {
    fn correlation_workload_id(&self) -> WorkloadId;
}

impl CorrelationWorkloadId for WorkloadLaunch {
    fn correlation_workload_id(&self) -> WorkloadId {
        WorkloadId(format!(
            "dotcore_development-6aedcbb34d83dcf1611bb92f048869885b01538d68d95978_{}_{}",
            self.correlation.owner_id, self.correlation.reconcile_generation
        ))
    }
}
