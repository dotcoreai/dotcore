//! Node model realizer acquisition lifecycle contracts with a controlled artifact preparer.

use std::time::Duration;

use dotcore_artifact::PreparationStep;
use dotcore_proto::dotcore::model::v1::{ModelDeploymentDesiredState, ModelDeploymentStatus};

use crate::model::controller::test_support::{AcquisitionControllerHarness, AcquisitionOutcome};

const ACQUIRING_STAGE: &str = "acquiring engine image";
const ACQUIRING_STATUS_MESSAGE: &str = "Acquiring engine image";
const ACQUISITION_FAILURE_STATUS_MESSAGE: &str = "Engine image acquisition failed";
const ACQUISITION_FAILURE_CODE: &str = "engine_image_acquisition_failed";

#[tokio::test]
async fn model_files_precede_engine_acquisition_and_runtime_create_waits_for_both() {
    let harness = AcquisitionControllerHarness::docker(&["deployment"]);
    harness.block_model_files("deployment");
    harness.block_acquisition("deployment");

    let reconciliation = harness.start_reconciliation();
    harness
        .wait_for_event("deployment", "model files preparing")
        .await;
    assert_eq!(harness.events(), ["deployment:model files preparing"]);
    assert_eq!(harness.runtime_create_count("deployment"), 0);
    assert_eq!(harness.acquisition_call_count("deployment"), 0);

    harness.release_model_files("deployment");
    harness.wait_for_event("deployment", ACQUIRING_STAGE).await;
    assert_eq!(
        harness.events(),
        [
            "deployment:model files preparing",
            "deployment:model files ready",
            "deployment:acquiring engine image",
        ]
    );
    assert_eq!(harness.runtime_create_count("deployment"), 0);
    let acquiring = harness.persisted_state("deployment");
    assert_eq!(
        ModelDeploymentStatus::try_from(acquiring.status),
        Ok(ModelDeploymentStatus::Loading)
    );
    assert_eq!(acquiring.status_message, ACQUIRING_STATUS_MESSAGE);
    assert_eq!(acquiring.progress, None);
    assert_eq!(acquiring.failure, None);

    harness.release_acquisition("deployment", AcquisitionOutcome::Ready);
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("reconciliation");
    assert_eq!(
        harness.events(),
        [
            "deployment:model files preparing",
            "deployment:model files ready",
            "deployment:acquiring engine image",
            "deployment:engine image ready",
            "deployment:runtime create",
        ]
    );
    assert_eq!(harness.acquisition_call_count("deployment"), 1);
    assert_eq!(harness.runtime_create_count("deployment"), 1);
}

#[tokio::test(start_paused = true)]
async fn lease_expiry_during_engine_preparation_rejects_completion_before_state_or_runtime_work() {
    let harness =
        AcquisitionControllerHarness::docker_leased(&["deployment"], Duration::from_secs(5));
    harness.block_acquisition("deployment");
    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("deployment", ACQUIRING_STAGE).await;
    let before_completion = harness.persisted_deployment("deployment");

    tokio::time::advance(Duration::from_secs(6)).await;
    harness.release_acquisition("deployment", AcquisitionOutcome::Ready);
    let error = reconciliation
        .await
        .expect("reconciliation task")
        .expect_err("expired preparation completion");

    assert!(matches!(
        error,
        super::ModelLifecycleError::Runtime(
            crate::dispatch::model::RuntimeTargetError::ActionRejected
        )
    ));
    assert_eq!(
        harness.persisted_deployment("deployment"),
        before_completion
    );
    assert_eq!(harness.runtime_create_count("deployment"), 0);
}

#[tokio::test]
async fn replacement_session_rejects_superseded_preparation_commit_and_forces_reacquisition() {
    let harness =
        AcquisitionControllerHarness::docker_leased(&["deployment"], Duration::from_secs(30));
    harness.block_acquisition("deployment");
    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("deployment", ACQUIRING_STAGE).await;
    let before_completion = harness.persisted_deployment("deployment");

    harness.replace_manager_session_with_same_view();
    harness.release_acquisition("deployment", AcquisitionOutcome::Ready);
    let error = reconciliation
        .await
        .expect("reconciliation task")
        .expect_err("superseded preparation completion");

    assert!(matches!(
        error,
        super::ModelLifecycleError::Runtime(
            crate::dispatch::model::RuntimeTargetError::StaleCompletion
        )
    ));
    assert_eq!(
        harness.persisted_deployment("deployment"),
        before_completion,
        "superseded preparation must not publish current state",
    );
    assert_eq!(harness.runtime_create_count("deployment"), 0);

    harness
        .reconcile_until_settled()
        .await
        .expect("replacement-session reconciliation");
    assert_eq!(
        harness.acquisition_call_count("deployment"),
        2,
        "superseded completion must not leave a prepared marker or reusable engine acquisition",
    );
    assert_eq!(harness.runtime_create_count("deployment"), 1);
}

#[tokio::test(start_paused = true)]
async fn exact_retry_and_unrelated_view_refresh_preserve_inflight_preparation() {
    let harness =
        AcquisitionControllerHarness::docker_leased(&["deployment"], Duration::from_secs(5));
    harness.block_acquisition("deployment");
    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("deployment", ACQUIRING_STAGE).await;

    tokio::time::advance(Duration::from_secs(4)).await;
    harness.refresh_current_view(1);
    harness.install_view_with_unrelated_deployment(2);
    tokio::time::advance(Duration::from_secs(4)).await;
    harness.release_acquisition("deployment", AcquisitionOutcome::Ready);
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("refreshed preparation completion");

    assert_eq!(
        harness.acquisition_call_count("deployment"),
        1,
        "exact and unrelated refreshes must not restart unchanged preparation",
    );
    assert_eq!(harness.runtime_create_count("deployment"), 1);
}

#[tokio::test(start_paused = true)]
async fn expired_view_withdraws_serving_without_mutating_the_running_workload() {
    let harness =
        AcquisitionControllerHarness::docker_leased(&["deployment"], Duration::from_secs(5));
    harness
        .reconcile_until_settled()
        .await
        .expect("initial ready reconciliation");
    assert_eq!(harness.serving_target_count(), 1);
    assert_eq!(harness.runtime_create_count("deployment"), 1);
    assert_eq!(harness.runtime_workload_count(), 1);

    tokio::time::advance(Duration::from_secs(6)).await;
    let error = harness
        .controller()
        .inner
        .reconcile()
        .await
        .expect_err("expired accepted view");

    assert!(matches!(
        error,
        super::ModelLifecycleError::Runtime(
            crate::dispatch::model::RuntimeTargetError::ActionRejected
        )
    ));
    assert_eq!(
        harness.serving_target_count(),
        0,
        "expired authority must withdraw existing serving publication",
    );
    assert_eq!(
        harness.runtime_create_count("deployment"),
        1,
        "expiry must not issue a replacement runtime mutation",
    );
    assert_eq!(
        harness.runtime_workload_count(),
        1,
        "the already-running workload must continue after expiry",
    );
}

#[tokio::test]
async fn acquisition_failure_is_persisted_per_deployment_without_observer_or_node_failure() {
    let harness = AcquisitionControllerHarness::docker(&["affected", "unaffected"]);
    harness.block_acquisition("affected");
    harness.set_acquisition_outcome("unaffected", AcquisitionOutcome::Ready);

    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("affected", ACQUIRING_STAGE).await;
    assert_eq!(harness.runtime_create_count("affected"), 0);
    harness.release_acquisition(
        "affected",
        AcquisitionOutcome::Failed("registry response containing private daemon detail"),
    );
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("isolated failure");

    assert_eq!(harness.runtime_create_count("affected"), 0);
    assert_eq!(harness.runtime_create_count("unaffected"), 1);
    let affected = harness.persisted_state("affected");
    assert_eq!(
        ModelDeploymentStatus::try_from(affected.status),
        Ok(ModelDeploymentStatus::Failed)
    );
    assert_eq!(affected.status_message, ACQUISITION_FAILURE_STATUS_MESSAGE);
    assert_eq!(affected.progress, None);
    let failure = affected.failure.as_ref().expect("failure summary");
    assert_eq!(failure.code, ACQUISITION_FAILURE_CODE);
    assert_eq!(
        failure.message,
        "Engine image acquisition failed; unload and deploy the model again"
    );
    assert_eq!(failure.diagnostics, Vec::<String>::new());
    assert!(!failure.message.contains("registry response"));
    assert_eq!(
        ModelDeploymentStatus::try_from(harness.persisted_state("unaffected").status),
        Ok(ModelDeploymentStatus::Ready)
    );
}

#[tokio::test]
async fn teardown_intent_cancels_acquisition_removes_state_and_fresh_deploy_reacquires() {
    let harness = AcquisitionControllerHarness::docker(&["deployment"]);
    harness.block_acquisition("deployment");
    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("deployment", ACQUIRING_STAGE).await;

    let unloaded = harness.unload("deployment");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(unloaded.desired_state),
        Ok(ModelDeploymentDesiredState::Teardown)
    );
    harness
        .wait_for_event("deployment", "engine image acquisition cancelled")
        .await;
    assert_eq!(harness.runtime_create_count("deployment"), 0);
    harness.finish_cancelled_acquisition("deployment");
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("cancelled reconciliation");
    harness
        .reconcile_until_settled()
        .await
        .expect("teardown reconciliation");
    assert_eq!(harness.persisted_deployment("deployment"), None);
    assert_eq!(harness.desired_deployment("deployment"), None);
    assert_eq!(harness.runtime_create_count("deployment"), 0);

    harness.set_next_deployment_id("deployment-fresh");
    harness.set_acquisition_outcome("deployment-fresh", AcquisitionOutcome::Ready);
    let loaded = harness.load();
    assert_eq!(loaded.model_deployment_id, "deployment-fresh");
    harness
        .reconcile_until_settled()
        .await
        .expect("fresh reconciliation");
    assert_eq!(harness.acquisition_call_count("deployment"), 1);
    assert_eq!(harness.acquisition_call_count("deployment-fresh"), 1);
    assert_eq!(harness.runtime_create_count("deployment-fresh"), 1);
}

#[tokio::test]
async fn persisted_failed_unload_removes_state_before_a_fresh_deploy() {
    let harness = AcquisitionControllerHarness::persisted_failed(
        "deployment",
        ACQUISITION_FAILURE_CODE,
        ACQUISITION_FAILURE_STATUS_MESSAGE,
    );
    let before_revision = harness.storage_revision();

    let unloaded = harness.unload("deployment");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(unloaded.desired_state),
        Ok(ModelDeploymentDesiredState::Teardown)
    );
    harness
        .reconcile_until_settled()
        .await
        .expect("failed teardown");
    assert_eq!(harness.persisted_deployment("deployment"), None);
    // Confirming cleanup removes the record in one transaction.
    assert_eq!(harness.storage_revision(), before_revision + 1);
    assert_eq!(harness.acquisition_call_count("deployment"), 0);
    assert_eq!(harness.runtime_create_count("deployment"), 0);
}

#[tokio::test]
async fn restart_reinspects_present_non_failed_deployments_but_never_retries_persisted_failed() {
    let harness = AcquisitionControllerHarness::restart(&[
        ("loading", ModelDeploymentStatus::Loading),
        ("ready", ModelDeploymentStatus::Ready),
        ("failed", ModelDeploymentStatus::Failed),
    ]);
    harness.block_acquisition("loading");
    harness.block_acquisition("ready");

    let reconciliation = harness.start_reconciliation();
    harness.wait_for_event("loading", ACQUIRING_STAGE).await;
    harness.wait_for_event("ready", ACQUIRING_STAGE).await;
    assert_eq!(harness.acquisition_call_count("loading"), 1);
    assert_eq!(harness.acquisition_call_count("ready"), 1);
    assert_eq!(harness.acquisition_call_count("failed"), 0);
    assert_eq!(harness.runtime_create_count("loading"), 0);
    assert_eq!(harness.runtime_create_count("ready"), 0);
    assert_eq!(
        ModelDeploymentStatus::try_from(harness.persisted_state("failed").status),
        Ok(ModelDeploymentStatus::Failed)
    );

    harness.release_acquisition("loading", AcquisitionOutcome::Ready);
    harness.release_acquisition("ready", AcquisitionOutcome::Ready);
    reconciliation
        .await
        .expect("reconciliation task")
        .expect("restart reconciliation");
    assert_eq!(harness.acquisition_call_count("failed"), 0);
    assert_eq!(harness.runtime_create_count("failed"), 0);
}

#[tokio::test]
async fn retagged_local_engine_image_still_prepares_and_launches_the_pinned_image_id() {
    let harness = AcquisitionControllerHarness::docker(&[]);
    let images = harness.local_images();
    images.tag(LOCAL_ENGINE_TAG, LOCAL_ENGINE_IMAGE_ID);
    accept_local_engine_deployment(&harness).await;

    images.tag(LOCAL_ENGINE_TAG, REBUILT_ENGINE_IMAGE_ID);
    harness
        .reconcile_until_settled()
        .await
        .expect("reconciliation");

    assert_eq!(
        images.preparation_requests(),
        [pinned_preparation_request()]
    );
    // The realizer launches the retained pin and never resolves the tag again.
    assert_eq!(images.resolved_references(), Vec::<String>::new());
    assert_eq!(harness.runtime_create_count("deployment-local"), 1);
    assert_eq!(
        harness.runtime_created_image("deployment-local").as_deref(),
        Some(LOCAL_ENGINE_IMAGE_ID)
    );
    assert_eq!(harness.persisted_state("deployment-local").failure, None);
}

#[tokio::test]
async fn removed_pinned_local_engine_image_fails_engine_image_acquisition() {
    let harness = AcquisitionControllerHarness::docker(&[]);
    let images = harness.local_images();
    images.tag(LOCAL_ENGINE_TAG, LOCAL_ENGINE_IMAGE_ID);
    accept_local_engine_deployment(&harness).await;

    images.tag(LOCAL_ENGINE_TAG, REBUILT_ENGINE_IMAGE_ID);
    images.remove_image(LOCAL_ENGINE_IMAGE_ID);
    harness
        .reconcile_until_settled()
        .await
        .expect("reconciliation");

    assert_eq!(
        images.preparation_requests(),
        [pinned_preparation_request()]
    );
    let failed = harness.persisted_state("deployment-local");
    assert_eq!(
        ModelDeploymentStatus::try_from(failed.status),
        Ok(ModelDeploymentStatus::Failed)
    );
    assert_eq!(failed.status_message, ACQUISITION_FAILURE_STATUS_MESSAGE);
    assert_eq!(
        failed.failure.expect("acquisition failure").code,
        ACQUISITION_FAILURE_CODE
    );
    assert_eq!(harness.runtime_create_count("deployment-local"), 0);
}

/// Declares `deployment-local` with the local-development engine image ID its node pinned when
/// the tag named it.
async fn accept_local_engine_deployment(harness: &AcquisitionControllerHarness) {
    let loaded = harness.load_pinned_local_engine("deployment-local", LOCAL_ENGINE_IMAGE_ID);
    assert_eq!(loaded.model_deployment_id, "deployment-local");
}

fn pinned_preparation_request() -> dotcore_artifact::OciPreparationRequest {
    dotcore_artifact::OciPreparationRequest {
        reference: LOCAL_ENGINE_IMAGE_ID.to_owned(),
        platform: dotcore_artifact::OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        },
        source: dotcore_artifact::OciPreparationSource::LocalImageId,
    }
}

const LOCAL_ENGINE_TAG: &str = "dotcore/engine:dev";
const LOCAL_ENGINE_IMAGE_ID: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const REBUILT_ENGINE_IMAGE_ID: &str =
    "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

#[tokio::test]
async fn engine_image_pulls_write_their_step_log_once_and_failures_say_why() {
    let harness = AcquisitionControllerHarness::docker(&["pulled", "failing"]);
    harness.set_acquisition_outcome("failing", AcquisitionOutcome::Failed("registry detail"));

    harness.reconcile_until_settled().await.expect("cold pass");
    let pulled = harness
        .preparation_log("pulled", PreparationStep::EngineImage)
        .await;
    let failing = harness
        .preparation_log("failing", PreparationStep::EngineImage)
        .await;
    let downloaded = harness
        .preparation_log("pulled", PreparationStep::ModelDownload)
        .await;
    harness.reconcile_until_settled().await.expect("warm pass");

    assert_eq!(
        pulled[..2],
        ["Preparing", "pulling the engine image of pulled"]
    );
    assert!(
        pulled[2].starts_with("Finished in ") && pulled.len() == 3,
        "{pulled:?}"
    );
    assert_eq!(
        failing,
        [
            "Preparing",
            "pulling the engine image of failing",
            "Failed: OCI image pull was rejected by the daemon: injected OCI daemon failure",
        ]
    );
    // The model files were already cached, so their step only states it; the engine is an OCI
    // image, so it has no build step. A settled deployment writes nothing more.
    assert_eq!(downloaded, [CACHED_MODEL_DECISION]);
    assert_eq!(
        harness.prepared_steps("pulled"),
        [PreparationStep::ModelDownload, PreparationStep::EngineImage]
    );
    assert_eq!(
        harness
            .preparation_log("pulled", PreparationStep::ModelDownload)
            .await,
        downloaded
    );
    assert_eq!(
        harness
            .preparation_log("pulled", PreparationStep::EngineImage)
            .await,
        pulled
    );
    assert_eq!(
        harness
            .preparation_log("failing", PreparationStep::EngineImage)
            .await,
        failing
    );
}

#[tokio::test]
async fn a_fully_warm_deployment_writes_one_decision_line_per_step_once() {
    let harness = AcquisitionControllerHarness::docker(&[]);
    harness
        .local_images()
        .tag(LOCAL_ENGINE_TAG, LOCAL_ENGINE_IMAGE_ID);
    accept_local_engine_deployment(&harness).await;

    harness.reconcile_until_settled().await.expect("first pass");
    let logs = async || {
        [
            harness
                .preparation_log("deployment-local", PreparationStep::ModelDownload)
                .await,
            harness
                .preparation_log("deployment-local", PreparationStep::EngineImage)
                .await,
        ]
    };
    let first = logs().await;
    harness
        .reconcile_until_settled()
        .await
        .expect("second pass");

    assert_eq!(
        first,
        [
            vec![CACHED_MODEL_DECISION.to_owned()],
            vec![format!("Already present: {LOCAL_ENGINE_IMAGE_ID}")],
        ]
    );
    // Steps list in the order they ran, the model download first.
    assert_eq!(
        harness.prepared_steps("deployment-local"),
        [PreparationStep::ModelDownload, PreparationStep::EngineImage]
    );
    assert_eq!(logs().await, first, "a repeated pass writes nothing more");
}

/// The decision line of the model artifact every fixture deployment finds already cached.
const CACHED_MODEL_DECISION: &str = "Already downloaded: dotcore/fixture/fixture-Q8_0.gguf@0123456789abcdef0123456789abcdef01234567";
