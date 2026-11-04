//! Proves manager-created model intent is realized by an independently provisioned worker process,
//! and that public inference sent to the manager reaches the worker-hosted model.
//!
//! The harness uses generated worker custody and real mutual TLS, a one-second worker reporter
//! interval, and a short injected worker capability maximum age, so lifecycle actions taken after
//! that age prove the worker admits them on refreshed observations. It inspects durable
//! per-process runtime and routing events so worker execution and zero manager execution remain
//! independently observable. The manager also reads, pages, and follows the worker-hosted model's
//! logs from the worker, its retained preparation steps included, while Ready and Paused, and
//! reports them unavailable while the worker is disconnected and not found once torn down, when
//! the worker has also removed its preparation logs. Another scenario simulates accelerator memory on
//! the worker to prove the worker attributes it, the manager lists it, and resource telemetry is
//! recorded on the worker only. A further scenario holds worker inference mid-attempt to prove
//! that the manager falls back only when the worker provably did no work, for example when the
//! worker is gone before the request reaches it, and never once output may have been produced.
//!
//! Every scenario needs the worker process to observe a real NVIDIA driver, which only Linux hosts
//! can provide, so the file is compiled for Linux only and skips at runtime without a driver.

#![cfg(target_os = "linux")]

use std::{fs, time::Duration};

use dotcore_proto::dotcore::{
    deployment::v1::{
        ExactPlacement, NodeResolvedLaunchIdentity,
        {
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation, AcceleratorMemoryUsageSet,
            accelerator_memory_observation, accelerator_memory_refresh_state,
        },
    },
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection,
        route_service_client::RouteServiceClient,
    },
    metrics::v1::{
        MetricAggregate, MetricQuery, MetricReduce, MetricSeries, QueryMetricsRequest,
        metric_point, metric_query, metrics_service_client::MetricsServiceClient,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DeploymentLogHistoryState,
        DeploymentLogResumePosition, DeploymentLogSourceState, DirectRouteExposure,
        FollowDeploymentLogsRequest, ListModelDeploymentsRequest, ModelDeployment,
        ModelDeploymentDesiredState, ModelDeploymentExposure, ModelDeploymentStatus,
        PauseModelRequest, ReadDeploymentLogsRequest, ReadDeploymentLogsResponse,
        RemoveModelDeploymentRequest, ResumeModelRequest, create_model_deployment_request,
        follow_deployment_logs_response, model_deployment_exposure,
        model_deployment_service_client::ModelDeploymentServiceClient, model_deployment_snapshot,
    },
    site::{store::v1::NodeCurrentState, v1::NodeObservationState},
    telemetry::v1::{WorkloadResourceObservationKind, WorkloadResourceRecord},
};
use dotcore_proto::google::protobuf::Duration as ProtoDuration;
use prost::Message;
use support::{ProcessCommand, ProcessTestControls};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::support;

const MILESTONE_DEADLINE: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
/// Injected worker capability maximum age; the site's reporter interval is one second.
const WORKER_CAPABILITY_MAX_AGE: Duration = Duration::from_secs(3);
/// Direct route through which the manager exposes the worker-hosted deployment.
const ROUTE_NAME: &str = "worker-model";
/// Lines the simulated runtime writes for each model workload.
const SIMULATED_WORKLOAD_LINES: u64 = 12;
/// Route trying the worker-hosted deployment first, then a manager-local one.
const FALLBACK_ROUTE_NAME: &str = "worker-then-manager";
/// Stream events a held worker stream sends before it holds: Started and the text delta.
const HELD_STREAM_EVENTS: usize = 2;
/// Deadline for the first resource telemetry record, sampled one hardware refresh interval after
/// readiness.
const TELEMETRY_DEADLINE: Duration = Duration::from_secs(30);
/// Worker action lease standing in for the production one, a few republications long.
const SHORT_ACTION_LEASE: Duration = Duration::from_secs(8);
/// Manager republish interval standing in for the production one, well inside the lease.
const SHORT_REPUBLISH_INTERVAL: Duration = Duration::from_secs(1);
/// Error a worker logs when a runtime action is refused because its action lease expired.
const FENCED_RUNTIME_ACTION: &str = "Runtime mutation was rejected by lifecycle fencing";

#[tokio::test]
async fn manager_created_model_is_realized_only_by_the_provisioned_worker_process() {
    if !support::nvidia_driver_observed() {
        return;
    }
    // The worker holds Downloading and Loading at its transition gates until each is observed.
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls::manual_clock(),
        ProcessTestControls {
            capability_max_age: Some(WORKER_CAPABILITY_MAX_AGE),
            model_transition_gates: true,
            ..ProcessTestControls::default()
        },
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    // Every collection from worker startup is now stale, so creation succeeds only on refreshed
    // worker observations.
    outlive_worker_capability_max_age().await;

    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    // The worker's simulated runtime acquires its assets first; the manager shows that step with
    // the runtime's own text and byte progress, as a manager-hosted deployment does.
    let manager_acquiring = follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Downloading,
        Some("Simulating model artifact download"),
    )
    .await;
    let acquiring_progress = manager_acquiring
        .state
        .and_then(|state| state.progress)
        .expect("worker asset acquisition lists byte progress");
    assert!(acquiring_progress.total.is_some_and(|total| total > 0));
    assert_eq!(
        acquiring_progress.percent,
        acquiring_progress
            .total
            .map(|total| (acquiring_progress.completed * 100 / total) as u32)
    );
    harness
        .worker_mut()
        .command(ProcessCommand::ReleasePreparationGate)
        .await
        .expect("release worker model preparation");
    let manager_loading =
        wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Loading).await;
    assert_eq!(
        manager_loading.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    let manager_loading_state = manager_loading.state.expect("manager Loading state");
    // The first Loading may still be the worker's Reconciling report for the new generation.
    assert!(
        matches!(
            manager_loading_state.status_message.as_str(),
            "Reconciling" | "Loading"
        ),
        "unexpected manager Loading message {:?}",
        manager_loading_state.status_message
    );
    assert!(manager_loading_state.observed_placements.is_empty());
    assert!(manager_loading_state.failure.is_none());
    harness
        .worker_mut()
        .command(ProcessCommand::ReleaseReadinessGate)
        .await
        .expect("release worker model readiness");

    if !wait_for_worker_status(
        harness.worker().node_state_path(),
        &deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await
    {
        harness.worker_mut().shutdown();
        harness.manager_mut().shutdown();
        panic!(
            "worker initial Ready deadline; worker logs at {}:\n{}\nmanager logs at {}:\n{}",
            harness.worker().site_root().join("logs").display(),
            support::retained_log_text(harness.worker().site_root()),
            harness.manager().site_root().join("logs").display(),
            support::retained_log_text(harness.manager().site_root())
        );
    }
    let manager_ready =
        wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;
    let worker_ready = support::retained_node_state(harness.worker().node_state_path())
        .model_deployments
        .into_iter()
        .find(|record| record.model_deployment_id == deployment_id)
        .and_then(|record| record.state)
        .expect("worker Ready state");
    assert_eq!(
        manager_ready.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    // The simulated worker target has no Docker runtime or accelerator, so memory does not apply;
    // in particular it never sits at a pending node observation.
    assert_eq!(
        manager_ready.accelerator_memory,
        Some(AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::NotApplicable(
                dotcore_proto::google::protobuf::Empty {}
            )),
        })
    );
    let manager_ready_state = manager_ready.state.expect("manager Ready state");
    assert_eq!(manager_ready_state.status_message, "Ready");
    assert!(manager_ready_state.failure.is_none());
    let ready_started_at = manager_ready_state
        .started_at
        .expect("worker-hosted record lists started_at");
    let ready_updated_at = manager_ready_state
        .updated_at
        .expect("worker-hosted record lists updated_at");
    assert!(
        (ready_updated_at.seconds, ready_updated_at.nanos)
            >= (ready_started_at.seconds, ready_started_at.nanos)
    );
    let manager_observed = manager_ready_state.observed_placements;
    assert_eq!(
        manager_observed, worker_ready.observed_placements,
        "manager status must preserve the worker-observed node and device values"
    );
    assert_eq!(manager_observed[0].managed_node_id, support::WORKER_NODE_ID);
    // Fresh worker evidence publishes a peer target, so the route is listed and the manager
    // forwards the routed request to the worker-hosted simulated model.
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    wait_for_listed_models(manager_inference, &[ROUTE_NAME]).await;
    assert_eq!(
        chat(manager_inference, ROUTE_NAME, false).await,
        (200, simulated_completion(ROUTE_NAME)),
        "the worker serves the routed request"
    );
    assert!(
        !String::from_utf8_lossy(
            &inference_bytes(manager_inference, "GET", "/v1/models", b"").await
        )
        .contains("127.0.0.2"),
        "the public model listing advertises no worker address"
    );

    // The manager reads and follows the worker-hosted deployment's logs from the worker.
    let newest = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager reads worker-hosted logs");
    let generation = simulated_log_generation(&newest);
    assert_simulated_log_page(&newest, &deployment_id, generation, 8..=12);
    assert_eq!(
        newest.history_state,
        DeploymentLogHistoryState::OlderOutputMayRemain as i32
    );
    let older = read_logs(&mut models, &deployment_id, newest.next_before_timestamp)
        .await
        .expect("manager reads an older worker-hosted page");
    assert_simulated_log_page(&older, &deployment_id, generation, 3..=7);
    assert_eq!(
        follow_log_contents(&mut models, &deployment_id, 10, 2).await,
        [11, 12]
            .map(|line| simulated_log_line(&deployment_id, generation, line))
            .to_vec(),
        "the follow resumes after line 10 and forwards the worker's lines"
    );

    // Losing the manager ends the public request path: the worker offers no public inference
    // listener or route management to fail over to.
    harness.manager_mut().stop();
    assert_connection_refused(manager_inference).await;
    let worker_addresses = harness.worker().addresses();
    assert_eq!(worker_addresses.inference, None);
    assert_eq!(worker_addresses.management, None);

    harness.manager_mut().restart();
    let restarted_manager = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(restarted_manager).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{restarted_manager}"))
        .await
        .expect("restarted manager model API connection");
    // The restarted manager holds no worker status yet; the transitions below start from Ready.
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;
    let pause_accepted = models
        .pause_model(PauseModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts pause after reconnect")
        .into_inner();
    assert_eq!(
        pause_accepted
            .deployment
            .and_then(|deployment| deployment.state)
            .expect("accepted pause state")
            .status,
        ModelDeploymentStatus::Pausing as i32
    );
    let manager_paused = follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Paused,
        None,
    )
    .await;
    assert!(
        wait_for_worker_status(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Paused,
        )
        .await
    );
    wait_for_listed_models(
        harness
            .manager()
            .addresses()
            .inference
            .expect("inference listener"),
        &[],
    )
    .await;
    assert_eq!(
        manager_paused.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    let manager_paused_state = manager_paused.state.expect("manager Paused state");
    assert_eq!(manager_paused_state.status_message, "Paused");
    // Logs do not depend on Ready: the Paused worker assignment is still read from the worker.
    let paused_logs = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager reads Paused worker-hosted logs");
    assert_simulated_log_page(&paused_logs, &deployment_id, generation, 8..=12);

    assert!(manager_paused_state.observed_placements.is_empty());
    assert!(manager_paused_state.failure.is_none());
    // A preparation step the worker retains reads and follows through the manager, ahead of the
    // workload source.
    dotcore_backend::testing::append_preparation_log(
        &harness.worker().site_root().join("node"),
        &deployment_id,
        "prepare:engine-build",
        // Older than every simulated workload line, so newest pages below stay unchanged.
        std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000),
        "building engine",
    );
    let all_sources = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager reads every worker-hosted source");
    assert_eq!(
        all_sources
            .sources
            .iter()
            .map(|source| (source.source_name.as_str(), source.source_label.as_str()))
            .collect::<Vec<_>>(),
        [("prepare:engine-build", "Engine build"), ("model", "Model")]
    );
    let step = models
        .read_deployment_logs(ReadDeploymentLogsRequest {
            model_deployment_id: deployment_id.clone(),
            source_names: vec!["prepare:engine-build".to_owned()],
            page_size: 5,
            ..Default::default()
        })
        .await
        .expect("manager reads a worker-hosted step")
        .into_inner();
    assert_eq!(
        step.entries
            .iter()
            .map(|entry| (entry.source_label.as_str(), entry.content.as_slice()))
            .collect::<Vec<_>>(),
        [("Engine build", b"building engine".as_slice())]
    );
    assert_eq!(
        follow_step_contents(&mut models, &deployment_id, "prepare:engine-build", 1).await,
        [b"building engine".to_vec()],
        "the manager follows the worker's step"
    );
    // Resume must also be admitted on a collection newer than any that creation could have used.
    outlive_worker_capability_max_age().await;
    let resume_accepted = models
        .resume_model(ResumeModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts resume after reconnect")
        .into_inner();
    assert_eq!(
        resume_accepted
            .deployment
            .and_then(|deployment| deployment.state)
            .expect("accepted resume state")
            .status,
        ModelDeploymentStatus::Loading as i32
    );
    let manager_resumed = follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Ready,
        None,
    )
    .await;
    assert!(
        wait_for_worker_status(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Ready,
        )
        .await
    );
    assert_eq!(
        manager_resumed.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    let manager_resumed_state = manager_resumed.state.expect("manager resumed Ready state");
    assert_eq!(manager_resumed_state.status_message, "Ready");
    assert_eq!(manager_resumed_state.observed_placements, manager_observed);
    assert!(manager_resumed_state.failure.is_none());

    harness.worker_mut().stop();
    harness.expire_silent_worker().await;
    support::wait_for_worker_observation(restarted_manager, NodeObservationState::Disconnected)
        .await;
    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts remote teardown while worker disconnected");
    let retained_teardown = models
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("manager model list while cleanup is disconnected")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .expect("manager retains teardown intent without worker absence evidence");
    assert_eq!(
        retained_teardown.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    let retained_teardown_state = retained_teardown.state.expect("manager teardown state");
    assert_eq!(
        retained_teardown_state.status,
        ModelDeploymentStatus::Unloading as i32
    );
    assert_eq!(retained_teardown_state.status_message, "Unloading");
    // A disconnected worker leaves the source unavailable instead of failing or hanging.
    let disconnected_logs = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager answers a log read while the worker is disconnected");
    assert!(disconnected_logs.entries.is_empty());
    assert_eq!(
        disconnected_logs
            .sources
            .iter()
            .map(|source| (source.source_name.as_str(), source.state))
            .collect::<Vec<_>>(),
        [("model", DeploymentLogSourceState::Unavailable as i32)]
    );
    assert!(retained_teardown_state.observed_placements.is_empty());
    assert!(retained_teardown_state.failure.is_none());
    assert!(
        std::fs::read_to_string(harness.manager().site_root().join("site.yaml"))
            .expect("manager site.yaml")
            .contains(&deployment_id),
        "manager cannot finalize cleanup before the disconnected worker reports exact absence"
    );

    harness.worker_mut().restart();
    support::wait_for_worker_connection(restarted_manager).await;
    wait_for_manager_absence(&mut models, &deployment_id).await;
    wait_for_worker_cleanup_retirement(harness.worker().node_state_path(), &deployment_id).await;
    assert!(
        !harness
            .worker()
            .site_root()
            .join("node/preparation-logs/model")
            .join(&deployment_id)
            .exists(),
        "the worker removes the deployment's preparation logs with its cleanup"
    );
    assert_eq!(
        read_logs(&mut models, &deployment_id, None)
            .await
            .expect_err("a torn-down deployment has no logs")
            .code(),
        tonic::Code::NotFound
    );

    harness.worker_mut().stop();
    harness.manager_mut().stop();

    let worker_records = support::retained_json_records(harness.worker().site_root());
    let first_valid_view = worker_records
        .iter()
        .position(|record| record["fields"]["event_kind"] == "node_desired_view_installed")
        .expect("worker installed a validated complete view");
    assert!(worker_records[..first_valid_view].iter().all(|record| {
        !matches!(
            record["fields"]["event_kind"].as_str(),
            Some("model_runtime_inspection" | "model_runtime_outcome")
        )
    }));
    assert_eq!(
        successful_runtime_operations(&worker_records, "create_workload"),
        1,
        "duplicate desired-view refreshes must not recreate the retained workload"
    );
    assert_eq!(
        successful_runtime_operations(&worker_records, "start_workload"),
        1,
        "the worker starts the retained workload only for the explicit Resume"
    );
    let ready_workload_ids = worker_records
        .iter()
        .filter(|record| record["fields"]["event_kind"] == "model_serving_target_ready")
        .filter_map(|record| record["fields"]["workload_id"].as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        ready_workload_ids,
        ["simulated-workload-1", "simulated-workload-1"],
        "pause, manager restart, and Resume must retain the original workload identity"
    );

    let worker_state = support::retained_node_state(harness.worker().node_state_path());
    assert!(
        worker_state
            .model_deployments
            .iter()
            .all(|record| record.model_deployment_id != deployment_id),
        "worker removes operational state after confirmed teardown"
    );
    assert!(
        worker_state
            .model_cleanup_evidence
            .iter()
            .all(|evidence| evidence.model_deployment_id != deployment_id),
        "worker retires durable cleanup evidence only after the exact manager receipt"
    );
    let manager_records = support::retained_json_records(harness.manager().site_root());
    assert!(
        !support::retained_log_text(harness.manager().site_root())
            .contains("Deployment log acquisition failed"),
        "expected worker log outcomes never log a manager error"
    );
    assert!(
        manager_records.iter().all(|record| {
            record["fields"]["event_kind"] != "model_serving_target_ready"
                || record["fields"]["deployment_id"] != deployment_id
        }),
        "remote Ready must never publish a manager-local serving target"
    );
    let peer_events = |event_kind: &str| {
        manager_records
            .iter()
            .filter(|record| {
                record["fields"]["event_kind"] == event_kind
                    && record["fields"]["deployment_id"] == deployment_id
                    && record["fields"]["node_id"] == support::WORKER_NODE_ID
            })
            .count()
    };
    assert!(
        peer_events("model_node_serving_target_published") >= 2,
        "remote Ready publishes a peer target before and after the manager restart"
    );
    assert!(
        peer_events("model_node_serving_target_withdrawn") >= 1,
        "pause withdraws the peer target"
    );
    assert!(
        manager_records.iter().all(|record| {
            record["fields"]["event_kind"] != "inference_fallback_exhausted"
                && record["fields"]["event_kind"] != "peer_inference_refused"
        }),
        "the routed request is served by the peer target without falling back"
    );
    assert_eq!(
        worker_records
            .iter()
            .filter(|record| {
                record["fields"]["event_kind"] == "peer_inference_attempt"
                    && record["fields"]["deployment_id"] == deployment_id
            })
            .map(|record| record["fields"]["outcome"].as_str())
            .collect::<Vec<_>>(),
        [Some("completed")],
        "the worker makes exactly one engine attempt for the forwarded request"
    );
    for operation in [
        "create_workload",
        "start_workload",
        "stop_workload",
        "remove_workload",
    ] {
        assert_eq!(
            successful_runtime_operations(&manager_records, operation),
            0,
            "manager must perform zero runtime mutations for the worker deployment"
        );
    }
    assert!(
        !std::fs::read_to_string(harness.manager().site_root().join("site.yaml"))
            .expect("manager site.yaml")
            .contains(&deployment_id),
        "manager must durably remove the teardown before acknowledging worker evidence"
    );
    let manager_state = support::retained_state(harness.manager().state_path());
    assert!(
        manager_state
            .manager_model_lifecycle_records
            .iter()
            .all(|record| record.deployment_id != deployment_id),
        "manager prunes the lifecycle record once the teardown removal is durable"
    );
    assert!(!harness.worker().site_root().join("site.yaml").exists());
}

#[tokio::test]
async fn one_follow_opened_before_preparation_receives_the_workload_and_its_end() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls::manual_clock(),
        ProcessTestControls {
            capability_max_age: Some(WORKER_CAPABILITY_MAX_AGE),
            model_transition_gates: true,
            ..ProcessTestControls::default()
        },
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    outlive_worker_capability_max_age().await;

    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    // The worker holds preparation at its gate, so the follow opens before any step or workload.
    follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Downloading,
        Some("Simulating model artifact download"),
    )
    .await;
    let mut follow = models
        .follow_deployment_logs(FollowDeploymentLogsRequest {
            model_deployment_id: deployment_id.clone(),
            ..Default::default()
        })
        .await
        .expect("manager follows every worker-hosted source")
        .into_inner();

    harness
        .worker_mut()
        .command(ProcessCommand::ReleasePreparationGate)
        .await
        .expect("release worker model preparation");
    harness
        .worker_mut()
        .command(ProcessCommand::ReleaseReadinessGate)
        .await
        .expect("release worker model readiness");
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;

    // The workload opens on the same stream, then its first lines arrive.
    let mut followed = FollowedSources::default();
    followed
        .read_until(&mut follow, |sources| sources.contents("model").len() >= 2)
        .await;
    let generation = simulated_log_generation(
        &read_logs(&mut models, &deployment_id, None)
            .await
            .expect("manager reads worker-hosted logs"),
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts removal");
    followed.read_to_end(&mut follow).await;
    wait_for_manager_absence(&mut models, &deployment_id).await;

    // The model is unavailable until the worker creates its workload, then opens on this stream.
    let mut expected = vec![
        FollowedEvent::State(DeploymentLogSourceState::Unavailable),
        FollowedEvent::State(DeploymentLogSourceState::Available),
    ];
    expected.extend(
        (1..=SIMULATED_WORKLOAD_LINES)
            .map(|line| FollowedEvent::Line(simulated_log_line(&deployment_id, generation, line))),
    );
    expected.push(FollowedEvent::State(DeploymentLogSourceState::Ended));
    assert_eq!(followed.by_source["model"], expected);
    assert_eq!(followed.by_source.len(), 1, "no other source is announced");

    harness.worker_mut().stop();
    harness.manager_mut().stop();
}

#[tokio::test]
async fn a_worker_held_at_a_model_transition_gate_still_shuts_down() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) =
        support::TwoProcessObservationHarness::start_with_worker_controls(ProcessTestControls {
            model_transition_gates: true,
            ..ProcessTestControls::default()
        })
    else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;

    // A process without gates answers the command with its refusal instead of leaving it pending.
    let refusal = harness
        .manager_mut()
        .command(ProcessCommand::ReleasePreparationGate)
        .await
        .expect_err("the manager has no transition gates");
    assert_eq!(
        refusal,
        "the subprocess refused ReleasePreparationGate: model transition gates are not enabled"
    );

    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Downloading,
        Some("Simulating model artifact download"),
    )
    .await;

    // The held preparation must not block graceful exit; shutdown fails past its deadline.
    assert!(harness.worker_mut().is_running());
    harness.worker_mut().shutdown();
}

#[tokio::test]
async fn a_worker_deployment_stays_controllable_past_its_action_lease_with_an_unchanged_view() {
    if !support::nvidia_driver_observed() {
        return;
    }
    // A short action lease and republish interval stand in for the production ones, keeping the
    // ratio that lets republications renew the lease.
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            view_republish_interval: Some(SHORT_REPUBLISH_INTERVAL),
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            model_transition_gates: true,
            node_action_lease: Some(SHORT_ACTION_LEASE),
            ..ProcessTestControls::default()
        },
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    follow_manager_status(
        &mut models,
        &deployment_id,
        ModelDeploymentStatus::Downloading,
        Some("Simulating model artifact download"),
    )
    .await;

    // Elapsed real time is the behavior here: the lease runs on the worker's monotonic clock,
    // and only republications of the unchanged view renew it while preparation is held.
    let worker_root = harness.worker().site_root().to_owned();
    let installed_views = support::installed_views(&worker_root);
    tokio::time::sleep(SHORT_ACTION_LEASE * 2).await;
    assert_eq!(
        support::installed_views(&worker_root),
        installed_views,
        "no changed view renewed the lease while preparation was held"
    );

    // Starting the workload after preparation needs a fresh lease.
    harness
        .worker_mut()
        .command(ProcessCommand::ReleasePreparationGate)
        .await
        .expect("release worker model preparation");
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Loading).await;
    harness
        .worker_mut()
        .command(ProcessCommand::ReleaseReadinessGate)
        .await
        .expect("release worker model readiness");
    assert!(
        wait_for_worker_status(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Ready,
        )
        .await,
        "the worker starts the workload past its action lease; worker logs:\n{}",
        support::retained_log_text(&worker_root)
    );
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;
    // An expired lease would have refused the start until a later publication renewed it.
    let fenced_actions = support::retained_json_records(&worker_root)
        .iter()
        .filter(|record| record["fields"]["error"] == FENCED_RUNTIME_ACTION)
        .count();
    assert_eq!(
        fenced_actions, 0,
        "the worker's action lease never expired while its view stayed unchanged"
    );
}

#[tokio::test]
async fn a_worker_pins_its_local_development_engine_once_across_its_lifecycle() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: DEVELOPMENT_ENGINE_CATALOG,
            ..ProcessTestControls::manual_clock()
        },
        ProcessTestControls::default(),
    ) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");

    let deployment_id = models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "fixture-dev-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(support::exact_placement(
                        support::WORKER_NODE_ID,
                        support::WORKER_MODEL_TARGET_ID,
                    )),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            )),
            exposure: Some(direct_exposure(ROUTE_NAME)),
            model_deployment_name: None,
        })
        .await
        .expect("manager accepts a worker-hosted local-development engine")
        .into_inner()
        .deployment
        .expect("accepted deployment")
        .model_deployment_id;
    let ready =
        wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;

    // The worker retained the pin before launching; the manager recorded it once and exposes it
    // beside the immutable snapshot, which keeps the tag without an identity. The worker's
    // simulated target pins a synthetic image ID, never asking its image store.
    let [
        NodeResolvedLaunchIdentity {
            selection_key,
            launch_identity: pinned,
        },
    ] = ready.node_resolved_launch_identities.as_slice()
    else {
        panic!("one pin: {:?}", ready.node_resolved_launch_identities);
    };
    assert_eq!(selection_key, "engine");
    assert!(
        dotcore_domain::oci::valid_local_image_id(pinned),
        "{pinned}"
    );
    let Some(model_deployment_snapshot::Source::Local(snapshot)) = ready
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    else {
        panic!("local snapshot");
    };
    assert_eq!(
        snapshot
            .engine_artifact
            .as_ref()
            .map(|engine| engine.launch_identity.as_str()),
        Some("")
    );
    assert_eq!(
        worker_engine_pins(&harness, &deployment_id),
        [pinned.as_str()]
    );

    models
        .pause_model(PauseModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts pause");
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Paused).await;
    models
        .resume_model(ResumeModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts resume");
    assert!(
        wait_for_worker_status(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Ready,
        )
        .await
    );

    // A restarted worker launches the retained pin again without resolving the tag.
    harness.worker_mut().stop();
    harness.expire_silent_worker().await;
    support::wait_for_worker_observation(manager_address, NodeObservationState::Disconnected).await;
    harness.worker_mut().restart();
    support::wait_for_worker_connection(manager_address).await;
    wait_for_listed_models(
        harness
            .manager()
            .addresses()
            .inference
            .expect("inference listener"),
        &[ROUTE_NAME],
    )
    .await;
    assert!(
        wait_for_worker_status(
            harness.worker().node_state_path(),
            &deployment_id,
            ModelDeploymentStatus::Ready,
        )
        .await
    );
    assert_eq!(
        worker_engine_pins(&harness, &deployment_id),
        [pinned.as_str()]
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_manager_absence(&mut models, &deployment_id).await;
    wait_for_worker_cleanup_retirement(harness.worker().node_state_path(), &deployment_id).await;
    assert!(
        worker_engine_pins(&harness, &deployment_id).is_empty(),
        "cleanup removes the retained pin with the deployment"
    );

    harness.worker_mut().stop();
    harness.manager_mut().stop();
    let pinned_events = support::retained_json_records(harness.worker().site_root())
        .into_iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "model_engine_launch_identity_pinned"
                && record["fields"]["deployment_id"] == deployment_id
        })
        .count();
    assert_eq!(
        pinned_events, 1,
        "the worker resolves the tag once across pause, resume, and restart"
    );
    let recorded_events = support::retained_json_records(harness.manager().site_root())
        .into_iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "manager_model_launch_identity"
                && record["fields"]["outcome"] == "recorded"
        })
        .count();
    assert_eq!(recorded_events, 1, "the manager records the pin once");
}

#[tokio::test]
async fn a_worker_on_a_declared_machine_reports_the_expected_memory_its_launch_received() {
    let Some(harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: &[("models/fixture-model.yaml", support::SIZED_FIXTURE_MODEL)],
            ..ProcessTestControls::default()
        },
        ProcessTestControls::default(),
    ) else {
        return;
    };
    let worker_machine = support::simulated_machine(
        support::WorkerModelTarget::SimulatedLinux
            .simulated_hardware()
            .expect("simulated worker hardware"),
    );
    let worker_gpu = worker_machine
        .gpu_ids()
        .next()
        .expect("worker GPU")
        .to_owned();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");

    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        support::hidden_model_exposure(),
    )
    .await;

    // The worker received the sized estimate in its launch over node control, and its declared
    // machine reports it: the manager lists that memory as the deployment's, and every value
    // listed on the way is a measurement or a typed unavailable reason, never a pending refresh.
    let memory = tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let memory = listed_deployment(&mut models, &deployment_id)
                .await
                .accelerator_memory
                .expect("listed accelerator memory");
            assert!(
                !memory.refresh.as_ref().is_some_and(|refresh| matches!(
                    refresh.outcome,
                    Some(accelerator_memory_refresh_state::Outcome::Pending(_))
                )),
                "worker-hosted memory is never a pending refresh: {memory:?}"
            );
            if matches!(
                memory.result,
                Some(accelerator_memory_observation::Result::Available(_))
            ) {
                return memory;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("measured worker memory deadline");
    assert_eq!(
        memory.result,
        Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet {
                devices: vec![AcceleratorMemoryDeviceUsage {
                    managed_node_id: support::WORKER_NODE_ID.to_owned(),
                    device_id: worker_gpu.clone(),
                    used_memory_bytes: support::SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES,
                }],
            }
        ))
    );
    assert!(memory.observed_at.is_some());

    // The worker's hardware, as the manager lists it, is its declared machine with the
    // deployment's estimate taken.
    let expected = worker_machine
        .render(
            &[dotcore_node_hardware::MachineWorkload {
                gpu_bytes: std::collections::BTreeMap::from([(
                    worker_gpu.clone(),
                    support::SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES,
                )]),
                host_bytes: support::SIZED_FIXTURE_MODEL_HOST_BYTES,
            }],
            std::time::UNIX_EPOCH,
        )
        .expect("expected worker hardware");
    support::wait_for_listed_hardware(manager_address, support::WORKER_NODE_ID, expected).await;

    // The worker records the attributed memory as history, which the manager serves.
    let history = deployment_memory_history(manager_address, &deployment_id).await;
    assert_eq!(
        history.labels,
        std::collections::HashMap::from([
            ("deployment".to_owned(), deployment_id.clone()),
            ("node".to_owned(), support::WORKER_NODE_ID.to_owned()),
            ("device".to_owned(), worker_gpu.clone()),
        ])
    );
    assert!(
        history.points.iter().all(|point| point.value
            == Some(metric_point::Value::Gauge(
                support::SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES as f64
            ))),
        "every recorded second holds the attributed bytes: {history:?}"
    );

    // Resource telemetry of the worker-hosted deployment is recorded on the worker only.
    let records = tokio::time::timeout(TELEMETRY_DEADLINE, async {
        loop {
            let records = deployment_telemetry(harness.worker().site_root(), &deployment_id);
            if !records.is_empty() {
                return records;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker telemetry deadline");
    let observation = records[0].observation.as_ref().expect("record observation");
    assert_eq!(
        observation.kind,
        WorkloadResourceObservationKind::Startup as i32
    );
    assert_eq!(
        observation.attributed_memory_bytes,
        support::SIZED_FIXTURE_MODEL_ACCELERATOR_BYTES
    );
    assert_eq!(
        records[0].node.as_ref().map(|node| node.node_id.as_str()),
        Some(support::WORKER_NODE_ID)
    );
    assert!(deployment_telemetry(harness.manager().site_root(), &deployment_id).is_empty());
}

#[tokio::test]
async fn manager_falls_back_from_a_worker_only_before_dispatch() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(mut harness) = support::TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls::manual_clock(),
        ProcessTestControls {
            held_inference_stream_events: Some(HELD_STREAM_EVENTS),
            ..ProcessTestControls::default()
        },
    ) else {
        return;
    };
    let manager = harness.manager().addresses();
    let manager_root = harness.manager().site_root().to_owned();
    let worker_root = harness.worker().site_root().to_owned();
    support::wait_for_worker_connection(manager.management.expect("management listener")).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let mut models = ModelDeploymentServiceClient::connect(format!(
        "http://{}",
        manager.management.expect("management listener")
    ))
    .await
    .expect("manager model API connection");
    let worker_deployment = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    let manager_deployment = create_local_deployment(
        &mut models,
        support::exact_placement("fixture-node", "fixture-target"),
        support::hidden_model_exposure(),
    )
    .await;
    RouteServiceClient::connect(format!(
        "http://{}",
        manager.management.expect("management listener")
    ))
    .await
    .expect("manager route API connection")
    .create_route(CreateRouteRequest {
        name: FALLBACK_ROUTE_NAME.to_owned(),
        enabled: true,
        fallback_groups: [&worker_deployment, &manager_deployment]
            .into_iter()
            .map(|deployment_id| FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec![deployment_id.clone()],
            })
            .collect(),
    })
    .await
    .expect("manager accepts the fallback route");
    wait_for_listed_models(
        manager.inference.expect("inference listener"),
        &[ROUTE_NAME, FALLBACK_ROUTE_NAME],
    )
    .await;
    let admitted = |record: &serde_json::Value| {
        record["fields"]["event_kind"] == "peer_inference_admission"
            && record["fields"]["outcome"] == "admitted"
    };
    let cancelled = |record: &serde_json::Value| {
        record["fields"]["event_kind"] == "peer_inference_attempt"
            && record["fields"]["outcome"] == "cancelled"
    };

    // Lost liveness: a frozen worker stops all contact, so the manager withdraws its target and
    // serves the fallback route from its own deployment until the worker recovers.
    harness.worker_mut().pause();
    harness.expire_silent_worker().await;
    wait_for_listed_models(
        manager.inference.expect("inference listener"),
        &[FALLBACK_ROUTE_NAME],
    )
    .await;
    wait_for_records(&manager_root, 1, |record| {
        record["fields"]["event_kind"] == "model_node_serving_target_withdrawn"
            && record["fields"]["reason"] == "node_not_connected"
    })
    .await;
    assert_eq!(
        chat(
            manager.inference.expect("inference listener"),
            ROUTE_NAME,
            false
        )
        .await
        .0,
        503
    );
    assert_eq!(
        chat(
            manager.inference.expect("inference listener"),
            FALLBACK_ROUTE_NAME,
            false
        )
        .await,
        (200, simulated_completion(FALLBACK_ROUTE_NAME))
    );
    harness.worker_mut().resume();
    wait_for_listed_models(
        manager.inference.expect("inference listener"),
        &[ROUTE_NAME, FALLBACK_ROUTE_NAME],
    )
    .await;

    // Worker gone before dispatch: its still-published target refuses the connection, so the
    // manager safely falls back. No earlier call opened a connection that could hide the refusal.
    harness.worker_mut().stop();
    assert_eq!(
        chat(
            manager.inference.expect("inference listener"),
            FALLBACK_ROUTE_NAME,
            false
        )
        .await,
        (200, simulated_completion(FALLBACK_ROUTE_NAME))
    );
    wait_for_records(&manager_root, 1, |record| {
        record["fields"]["event_kind"] == "peer_inference_refused"
            && record["fields"]["reason"] == "connect_failed"
    })
    .await;
    restart_stopped_worker(&mut harness).await;

    // Manager cancellation: a client that disconnects mid-attempt cancels the worker attempt.
    let client = send_chat(
        manager.inference.expect("inference listener"),
        FALLBACK_ROUTE_NAME,
        false,
    )
    .await;
    wait_for_records(&worker_root, 1, admitted).await;
    drop(client);
    wait_for_records(&worker_root, 1, cancelled).await;

    // Worker binding revocation: pausing the deployment cancels the attempt it already started,
    // and the manager reports the revocation instead of falling back.
    let revoked = tokio::spawn(chat(
        manager.inference.expect("inference listener"),
        FALLBACK_ROUTE_NAME,
        false,
    ));
    wait_for_records(&worker_root, 2, admitted).await;
    models
        .pause_model(PauseModelRequest {
            model_deployment_id: worker_deployment.clone(),
        })
        .await
        .expect("manager accepts pause during an attempt");
    assert_eq!(
        revoked.await.expect("revoked request").0,
        503,
        "a revoked attempt is not retried on the manager deployment"
    );
    wait_for_records(&worker_root, 2, cancelled).await;
    models
        .resume_model(ResumeModelRequest {
            model_deployment_id: worker_deployment.clone(),
        })
        .await
        .expect("manager accepts resume");
    wait_for_listed_models(
        manager.inference.expect("inference listener"),
        &[ROUTE_NAME, FALLBACK_ROUTE_NAME],
    )
    .await;

    // Worker gone after dispatch: the attempt may have run, so the manager reports the failure
    // instead of falling back.
    let interrupted = tokio::spawn(chat(
        manager.inference.expect("inference listener"),
        FALLBACK_ROUTE_NAME,
        false,
    ));
    wait_for_records(&worker_root, 3, admitted).await;
    harness.worker_mut().stop();
    let (status, body) = interrupted.await.expect("interrupted request");
    assert_eq!(status, 502);
    assert_eq!(body["error"]["type"], "upstream_error");
    restart_stopped_worker(&mut harness).await;

    // Worker gone after stream output: the committed stream ends with an error frame and without
    // [DONE], and is never replayed on the manager deployment.
    let mut stream = send_chat(
        manager.inference.expect("inference listener"),
        FALLBACK_ROUTE_NAME,
        true,
    )
    .await;
    let mut received = read_until(&mut stream, b"Simulated").await;
    harness.worker_mut().stop();
    tokio::time::timeout(MILESTONE_DEADLINE, stream.read_to_end(&mut received))
        .await
        .expect("truncated stream deadline")
        .expect("truncated stream");
    let received = String::from_utf8_lossy(&received);
    assert!(received.contains("upstream_error"), "{received}");
    assert!(!received.contains("[DONE]"), "{received}");

    harness.manager_mut().shutdown();
    let manager_records = support::retained_json_records(&manager_root);
    let attempts_on = |deployment_id: &str| {
        manager_records
            .iter()
            .filter(|record| {
                record["fields"]["event_kind"] == "inference_target_attempt"
                    && record["fields"]["deployment_id"] == deployment_id
            })
            .count()
    };
    assert_eq!(
        attempts_on(&worker_deployment),
        5,
        "each request after recovery tried the worker exactly once"
    );
    assert_eq!(
        attempts_on(&manager_deployment),
        2,
        "only the stale-worker request and the pre-dispatch refusal reached the manager deployment"
    );
    assert_eq!(
        manager_records
            .iter()
            .filter(|record| record["fields"]["event_kind"] == "inference_fallback_selected")
            .map(|record| record["fields"]["deployment_id"].as_str())
            .collect::<Vec<_>>(),
        [Some(manager_deployment.as_str())],
        "only the pre-dispatch refusal falls back"
    );
    assert_eq!(
        manager_records
            .iter()
            .filter(|record| {
                record["fields"]["event_kind"] == "model_node_serving_target_withdrawn"
                    && record["fields"]["deployment_id"] == manager_deployment.as_str()
            })
            .count(),
        0,
        "advancing the manager clock never withdrew the live manager's own target"
    );
}

/// Engine launch identities the worker retains for one deployment.
#[tokio::test]
async fn a_split_network_worker_serves_inference_and_logs_through_the_manager() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(harness) = support::TwoProcessObservationHarness::start_split(
        ProcessTestControls::default(),
        ProcessTestControls::default(),
        &[],
    ) else {
        return;
    };
    // The site records both views of the worker, and the worker dials the manager at its own.
    let worker = harness
        .manager_site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == support::WORKER_NODE_ID)
        .expect("declared worker");
    assert_eq!(
        (worker.host, worker.manager_host),
        (
            support::SPLIT_WORKER_HOST.to_string(),
            support::SPLIT_MANAGER_HOST.to_string()
        )
    );
    let manager_private = harness
        .manager()
        .addresses()
        .private
        .expect("manager private listener");
    assert_eq!(
        harness.node_folder().manager_endpoint().as_str(),
        format!(
            "https://{}:{}/",
            support::SPLIT_MANAGER_HOST,
            manager_private.port()
        )
    );
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;

    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;

    // The manager forwards inference and reads logs at the worker's declared address.
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    wait_for_listed_models(manager_inference, &[ROUTE_NAME]).await;
    assert_eq!(
        chat(manager_inference, ROUTE_NAME, false).await,
        (200, simulated_completion(ROUTE_NAME)),
        "the worker serves the routed request across the split network"
    );
    let newest = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager reads worker-hosted logs across the split network");
    let generation = simulated_log_generation(&newest);
    assert_simulated_log_page(&newest, &deployment_id, generation, 8..=12);
    assert_eq!(
        follow_log_contents(&mut models, &deployment_id, 10, 2).await,
        [11, 12]
            .map(|line| simulated_log_line(&deployment_id, generation, line))
            .to_vec(),
        "the follow forwards the worker's lines across the split network"
    );

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_manager_absence(&mut models, &deployment_id).await;
}

/// A worker declared by host name serves inference and logs through a manager declared by address.
#[tokio::test]
async fn a_worker_declared_by_host_name_serves_inference_and_logs_through_the_manager() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(harness) = support::TwoProcessObservationHarness::start_named_worker(
        ProcessTestControls::default(),
        ProcessTestControls::default(),
    ) else {
        return;
    };
    // The site records the worker's name and no manager host, since the token named the manager.
    let worker = harness
        .manager_site()
        .nodes
        .into_iter()
        .find(|node| node.node_id == support::WORKER_NODE_ID)
        .expect("declared worker");
    assert_eq!(
        (worker.host, worker.manager_host),
        (support::WORKER_HOST_NAME.to_owned(), String::new())
    );
    let manager_private = harness
        .manager()
        .addresses()
        .private
        .expect("manager private listener");
    assert_eq!(
        harness.node_folder().manager_endpoint().as_str(),
        format!("https://{manager_private}/")
    );
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;

    let mut models = ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager model API connection");
    let deployment_id = create_local_deployment(
        &mut models,
        support::exact_placement(support::WORKER_NODE_ID, support::WORKER_MODEL_TARGET_ID),
        direct_exposure(ROUTE_NAME),
    )
    .await;
    wait_for_manager_status(&mut models, &deployment_id, ModelDeploymentStatus::Ready).await;

    // The manager dials the worker at `localhost` for inference and logs.
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    wait_for_listed_models(manager_inference, &[ROUTE_NAME]).await;
    assert_eq!(
        chat(manager_inference, ROUTE_NAME, false).await,
        (200, simulated_completion(ROUTE_NAME)),
        "the worker declared by name serves the routed request"
    );
    let newest = read_logs(&mut models, &deployment_id, None)
        .await
        .expect("manager reads worker-hosted logs through the host name");
    let generation = simulated_log_generation(&newest);
    assert_simulated_log_page(&newest, &deployment_id, generation, 8..=12);

    models
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_manager_absence(&mut models, &deployment_id).await;
}

fn worker_engine_pins(
    harness: &support::TwoProcessObservationHarness,
    deployment_id: &str,
) -> Vec<String> {
    support::retained_node_state(harness.worker().node_state_path())
        .model_resolved_launch_identities
        .into_iter()
        .filter(|record| record.deployment_id == deployment_id)
        .flat_map(|record| record.resolved_launch_identities)
        .map(|pin| pin.launch_identity)
        .collect()
}

/// Manager site catalog additions: an engine release declared by tag only, and a model using it.
const DEVELOPMENT_ENGINE_CATALOG: &[(&str, &str)] = &[
    (
        "engines/fixture-dev.yaml",
        r#"engineReleaseId: "fixture-dev-engine"
engineKind: "ENGINE_KIND_LLAMA_CPP"
version: "1"
parameterSchema:
  schemaId: "fixture-dev-schema"
  parameters:
    - parameterId: "total-context"
      displayName: "Total context"
      exposed: true
      unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" }
    - parameterId: "context-per-request"
      displayName: "Context per request"
      exposed: true
      unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens" }
    - parameterId: "parallel-requests"
      displayName: "Parallel requests"
      exposed: true
      unsignedInteger: { minimum: "1", maximum: "64", unit: "requests", defaultValue: "4" }
    - parameterId: "prompt-cache-memory-mib"
      displayName: "Prompt cache memory"
      unsignedInteger: { minimum: "0", maximum: "65536", unit: "MiB" }
artifacts:
  - artifactId: "fixture-dev-linux"
    oci:
      repository:
        registry: { host: "registry.dotcore.test" }
        name: "fixture-dev"
      tag: "dev"
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
      - platform: { operatingSystem: "linux", architecture: "arm64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#,
    ),
    (
        "models/fixture-dev-model.yaml",
        r#"modelId: "fixture-dev-model"
displayName: "fixture development model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "fixture-artifact"
    weightQuantization: "Q8_0"
    source:
      repository: "dotcore/fixture"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture-Q8_0.gguf"
        provenanceDigest: "0000000000000000000000000000000000000000000000000000000000000000"
configurations:
  - configurationId: "fixture-configuration"
    engineArtifacts: [{ engineReleaseId: "fixture-dev-engine", engineArtifactId: "fixture-dev-linux" }]
    artifactBindings: [{ bindingId: "fixture-artifact", modelArtifactId: "fixture-artifact" }]
    hardwareSupport:
      - hardwareClassId: "nvidia"
        hardwareRequirements: { minimumSystemMemoryBytes: "1" }
    engineConfiguration:
      engineKind: "ENGINE_KIND_LLAMA_CPP"
      llamaCpp: { fixedSettings: { gpuLayers: 0 } }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
profiles:
  - profileId: "fixture-profile"
    displayName: "Fixture profile"
    description: "Launches the local-development fixture engine."
    configurationId: "fixture-configuration"
    defaultModelArtifactBindingId: "fixture-artifact"
    engineParameterDefaults:
      values:
        - { parameterId: "prompt-cache-memory-mib", unsignedInteger: "0" }
"#,
    ),
];

/// One event of a followed source: a state change or an output line.
#[derive(Debug, PartialEq)]
enum FollowedEvent {
    State(DeploymentLogSourceState),
    Line(Vec<u8>),
}

/// The events one all-sources follow delivered, in order within each source.
#[derive(Default)]
struct FollowedSources {
    by_source: std::collections::BTreeMap<String, Vec<FollowedEvent>>,
}

impl FollowedSources {
    /// Reads events until `done` holds, failing at the milestone deadline.
    async fn read_until(
        &mut self,
        follow: &mut tonic::Streaming<
            dotcore_proto::dotcore::model::v1::FollowDeploymentLogsResponse,
        >,
        done: impl Fn(&Self) -> bool,
    ) {
        tokio::time::timeout(MILESTONE_DEADLINE, async {
            while !done(self) {
                let message = follow
                    .message()
                    .await
                    .expect("follow stays healthy")
                    .expect("follow stays open");
                self.record(message);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("follow progress deadline: {:?}", self.by_source));
    }

    /// Reads events until the stream ends cleanly, failing at the milestone deadline.
    async fn read_to_end(
        &mut self,
        follow: &mut tonic::Streaming<
            dotcore_proto::dotcore::model::v1::FollowDeploymentLogsResponse,
        >,
    ) {
        tokio::time::timeout(MILESTONE_DEADLINE, async {
            while let Some(message) = follow.message().await.expect("follow ends without error") {
                self.record(message);
            }
        })
        .await
        .unwrap_or_else(|_| panic!("follow end deadline: {:?}", self.by_source));
    }

    /// The output lines a source delivered so far.
    fn contents(&self, source_name: &str) -> Vec<&[u8]> {
        self.by_source
            .get(source_name)
            .into_iter()
            .flatten()
            .filter_map(|event| match event {
                FollowedEvent::Line(content) => Some(content.as_slice()),
                FollowedEvent::State(_) => None,
            })
            .collect()
    }

    fn record(&mut self, message: dotcore_proto::dotcore::model::v1::FollowDeploymentLogsResponse) {
        let (source_name, event) = match message.event.expect("follow event") {
            follow_deployment_logs_response::Event::SourceEvent(event) => (
                event.source_name,
                FollowedEvent::State(
                    DeploymentLogSourceState::try_from(event.state).expect("known source state"),
                ),
            ),
            follow_deployment_logs_response::Event::Entry(entry) => {
                (entry.source_name, FollowedEvent::Line(entry.content))
            }
        };
        self.by_source.entry(source_name).or_default().push(event);
    }
}

/// Reads the newest five log entries of a deployment through the manager, before an optional
/// whole-second boundary.
async fn read_logs(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    before_timestamp: Option<dotcore_proto::google::protobuf::Timestamp>,
) -> Result<ReadDeploymentLogsResponse, tonic::Status> {
    models
        .read_deployment_logs(ReadDeploymentLogsRequest {
            model_deployment_id: deployment_id.to_owned(),
            page_size: 5,
            before_timestamp,
            ..Default::default()
        })
        .await
        .map(tonic::Response::into_inner)
}

/// Follows a deployment's logs through the manager after simulated line `after`, returning the
/// first `count` entries, then drops the follow.
async fn follow_log_contents(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    after: u64,
    count: usize,
) -> Vec<Vec<u8>> {
    let mut follow = models
        .follow_deployment_logs(FollowDeploymentLogsRequest {
            model_deployment_id: deployment_id.to_owned(),
            resume_positions: vec![DeploymentLogResumePosition {
                source_name: "model".to_owned(),
                latest_timestamp: Some(simulated_log_timestamp(after)),
            }],
            ..Default::default()
        })
        .await
        .expect("manager follows worker-hosted logs")
        .into_inner();
    let mut contents = Vec::new();
    while contents.len() < count {
        let event = tokio::time::timeout(MILESTONE_DEADLINE, follow.message())
            .await
            .expect("follow event deadline")
            .expect("follow stream")
            .expect("follow stays open")
            .event
            .expect("follow event");
        match event {
            follow_deployment_logs_response::Event::SourceEvent(event) => assert_eq!(
                (event.source_name.as_str(), event.state),
                ("model", DeploymentLogSourceState::Available as i32)
            ),
            follow_deployment_logs_response::Event::Entry(entry) => {
                assert_eq!(entry.source_name, "model");
                contents.push(entry.content);
            }
        }
    }
    contents
}

/// Follows one preparation step of a deployment through the manager from its first entry,
/// returning the first `count` entries, then drops the follow.
async fn follow_step_contents(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    source_name: &str,
    count: usize,
) -> Vec<Vec<u8>> {
    let mut follow = models
        .follow_deployment_logs(FollowDeploymentLogsRequest {
            model_deployment_id: deployment_id.to_owned(),
            source_names: vec![source_name.to_owned()],
            ..Default::default()
        })
        .await
        .expect("manager follows a worker-hosted step")
        .into_inner();
    let mut contents = Vec::new();
    while contents.len() < count {
        let event = tokio::time::timeout(MILESTONE_DEADLINE, follow.message())
            .await
            .expect("follow event deadline")
            .expect("follow stream")
            .expect("follow stays open")
            .event
            .expect("follow event");
        match event {
            follow_deployment_logs_response::Event::SourceEvent(event) => assert_eq!(
                (event.source_name.as_str(), event.state),
                (source_name, DeploymentLogSourceState::Available as i32)
            ),
            follow_deployment_logs_response::Event::Entry(entry) => {
                assert_eq!(entry.source_name, source_name);
                contents.push(entry.content);
            }
        }
    }
    contents
}

/// Asserts a page holds exactly the simulated lines in `lines`, with their timestamps.
fn assert_simulated_log_page(
    page: &ReadDeploymentLogsResponse,
    deployment_id: &str,
    generation: u64,
    lines: std::ops::RangeInclusive<u64>,
) {
    assert_eq!(
        page.entries
            .iter()
            .map(|entry| (entry.timestamp, entry.content.clone()))
            .collect::<Vec<_>>(),
        lines
            .map(|line| (
                Some(simulated_log_timestamp(line)),
                simulated_log_line(deployment_id, generation, line)
            ))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        page.sources
            .iter()
            .map(|source| (source.source_name.as_str(), source.state))
            .collect::<Vec<_>>(),
        [("model", DeploymentLogSourceState::Available as i32)]
    );
}

/// Returns the reconcile generation the simulated runtime printed in a page's first line.
fn simulated_log_generation(page: &ReadDeploymentLogsResponse) -> u64 {
    let first = page.entries.first().expect("a simulated log line");
    String::from_utf8_lossy(&first.content)
        .rsplit_once("generation=")
        .and_then(|(_, generation)| generation.parse().ok())
        .expect("Simulated line names its generation")
}

/// The simulated runtime's deterministic log line `line` of one workload.
fn simulated_log_line(deployment_id: &str, generation: u64, line: u64) -> Vec<u8> {
    format!("simulated workload log line {line:03} owner={deployment_id} generation={generation}")
        .into_bytes()
}

/// The simulated runtime's timestamp of log line `line`: two seconds apart from a fixed epoch.
fn simulated_log_timestamp(line: u64) -> dotcore_proto::google::protobuf::Timestamp {
    dotcore_proto::google::protobuf::Timestamp {
        seconds: 1_700_000_000 + 2 * line as i64,
        nanos: 0,
    }
}

/// Waits until the manager's public `/v1/models` lists exactly `expected`.
async fn wait_for_listed_models(address: std::net::SocketAddr, expected: &[&str]) {
    tokio::time::timeout(support::LIVENESS_EXPIRY_DEADLINE, async {
        loop {
            let (status, body) = inference_request(address, "GET", "/v1/models", b"").await;
            let listed = body["data"]
                .as_array()
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|model| model["id"].as_str())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if status == 200
                && listed.len() == expected.len()
                && expected.iter().all(|route| listed.contains(route))
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager model listing deadline");
}

/// Restarts a stopped worker once the manager has withdrawn its old target, so later requests
/// reach the new worker process rather than the stopped one.
async fn restart_stopped_worker(harness: &mut support::TwoProcessObservationHarness) {
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    harness.expire_silent_worker().await;
    wait_for_listed_models(manager_inference, &[FALLBACK_ROUTE_NAME]).await;
    harness.worker_mut().restart();
    wait_for_listed_models(manager_inference, &[ROUTE_NAME, FALLBACK_ROUTE_NAME]).await;
}

/// Creates one local simulated deployment of the fixture model and returns its id.
async fn create_local_deployment(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    placement: ExactPlacement,
    exposure: ModelDeploymentExposure,
) -> String {
    let deployment = models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(create_model_deployment_request::Source::Local(
                CreateLocalModelDeploymentRequest {
                    model_id: "fixture-model".to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(placement),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            )),
            exposure: Some(exposure),
            model_deployment_name: None,
        })
        .await
        .expect("manager accepts the local deployment")
        .into_inner();
    deployment
        .deployment
        .expect("accepted deployment")
        .model_deployment_id
}

fn direct_exposure(route_name: &str) -> ModelDeploymentExposure {
    ModelDeploymentExposure {
        kind: Some(model_deployment_exposure::Kind::Direct(
            DirectRouteExposure {
                route_name: route_name.to_owned(),
            },
        )),
    }
}

/// The public completion the simulated model returns through `route`.
fn simulated_completion(route: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl_simulated",
        "object": "chat.completion",
        "created": 0,
        "model": route,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "Simulated response."
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 1,
            "completion_tokens": 3,
            "total_tokens": 4
        }
    })
}

/// Sends one chat completion through `route` and returns its status and JSON body.
async fn chat(
    address: std::net::SocketAddr,
    route: &str,
    stream: bool,
) -> (u16, serde_json::Value) {
    inference_request(
        address,
        "POST",
        "/v1/chat/completions",
        &chat_body(route, stream),
    )
    .await
}

/// Sends one chat completion through `route` and returns the open connection unread.
async fn send_chat(
    address: std::net::SocketAddr,
    route: &str,
    stream: bool,
) -> tokio::net::TcpStream {
    send_request(
        address,
        "POST",
        "/v1/chat/completions",
        &chat_body(route, stream),
    )
    .await
}

fn chat_body(route: &str, stream: bool) -> Vec<u8> {
    format!(
        r#"{{"model":"{route}","stream":{stream},"messages":[{{"role":"user","content":"hello"}}]}}"#
    )
    .into_bytes()
}

/// Reads from `stream` until the received bytes contain `marker`, and returns them.
async fn read_until(stream: &mut tokio::net::TcpStream, marker: &[u8]) -> Vec<u8> {
    let mut received = Vec::new();
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        let mut buffer = [0; 4096];
        while !received
            .windows(marker.len())
            .any(|window| window == marker)
        {
            let read = stream.read(&mut buffer).await.expect("stream read");
            assert_ne!(read, 0, "stream closed before {marker:?}");
            received.extend_from_slice(&buffer[..read]);
        }
    })
    .await
    .expect("stream output deadline");
    received
}

async fn assert_connection_refused(address: std::net::SocketAddr) {
    let error = tokio::net::TcpStream::connect(address)
        .await
        .expect_err("no listener at the address");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
}

/// Waits until the retained logs under `site_root` hold at least `count` matching records.
async fn wait_for_records(
    site_root: &std::path::Path,
    count: usize,
    matches: impl Fn(&serde_json::Value) -> bool,
) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        while support::retained_json_records(site_root)
            .iter()
            .filter(|record| matches(record))
            .count()
            < count
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("retained log record deadline");
}

/// Sends one HTTP/1.1 request to a public inference listener and returns its status and JSON body.
async fn inference_request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> (u16, serde_json::Value) {
    let response = inference_bytes(address, method, path, body).await;
    let end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("inference response headers");
    let status = String::from_utf8_lossy(&response[..end])
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("inference response status");
    let body = serde_json::from_slice(&response[end + 4..]).expect("inference JSON body");
    (status, body)
}

/// Sends one HTTP/1.1 request and returns the complete raw response.
async fn inference_bytes(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> Vec<u8> {
    let mut stream = send_request(address, method, path, body).await;
    let mut response = Vec::new();
    tokio::time::timeout(MILESTONE_DEADLINE, stream.read_to_end(&mut response))
        .await
        .expect("inference response deadline")
        .expect("inference response");
    response
}

async fn send_request(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("inference connection");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(head.as_bytes())
        .await
        .expect("inference request head");
    stream
        .write_all(body)
        .await
        .expect("inference request body");
    stream
}

async fn outlive_worker_capability_max_age() {
    tokio::time::sleep(WORKER_CAPABILITY_MAX_AGE + Duration::from_secs(1)).await;
}

/// Returns the manager's listed deployment.
async fn listed_deployment(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
) -> ModelDeployment {
    models
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("manager model list")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .expect("listed deployment")
}

/// Reads the workload resource records a backend wrote under its site root for one deployment;
/// a backend that wrote none has no telemetry directory.
/// Waits until the manager serves accelerator memory history of `deployment_id`, then returns its
/// only series.
async fn deployment_memory_history(
    manager_address: std::net::SocketAddr,
    deployment_id: &str,
) -> MetricSeries {
    let mut metrics = MetricsServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager metrics API connection");
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let result = metrics
                .query_metrics(QueryMetricsRequest {
                    query: Some(MetricQuery {
                        metric: "deployment.accelerator.memory.used".to_owned(),
                        matchers: std::collections::HashMap::from([(
                            "deployment".to_owned(),
                            deployment_id.to_owned(),
                        )]),
                        group_by: vec![
                            "deployment".to_owned(),
                            "node".to_owned(),
                            "device".to_owned(),
                        ],
                        reduce: MetricReduce::Last as i32,
                        aggregate: MetricAggregate::Sum as i32,
                        range: Some(metric_query::Range::Last(ProtoDuration {
                            seconds: 60,
                            nanos: 0,
                        })),
                        step_hint: Some(ProtoDuration {
                            seconds: 1,
                            nanos: 0,
                        }),
                        ..MetricQuery::default()
                    }),
                })
                .await
                .expect("deployment memory history query")
                .into_inner()
                .result
                .expect("query result");
            if result.series.iter().any(|series| !series.points.is_empty()) {
                assert_eq!(result.series.len(), 1, "one series: {result:?}");
                return result.series.into_iter().next().expect("one series");
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("deployment memory history deadline")
}

fn deployment_telemetry(
    site_root: &std::path::Path,
    deployment_id: &str,
) -> Vec<WorkloadResourceRecord> {
    let directory = site_root.join("telemetry");
    if !directory.is_dir() {
        return Vec::new();
    }
    dotcore_backend::telemetry::WorkloadResourceRecords::read(&[directory])
        .expect("telemetry records")
        .records
        .into_iter()
        .filter(|record| {
            record
                .realization
                .as_ref()
                .is_some_and(|realization| realization.owner_id == deployment_id)
        })
        .collect()
}

async fn wait_for_manager_status(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
) -> ModelDeployment {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let deployments = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model status")
                .into_inner()
                .deployments;
            if let Some(deployment) = deployments.into_iter().find(|deployment| {
                deployment.model_deployment_id == deployment_id
                    && deployment
                        .state
                        .as_ref()
                        .is_some_and(|state| state.status == expected as i32)
            }) {
                return deployment;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager model status deadline")
}

/// Samples the manager list until the deployment reaches `expected`, with `expected_message` when
/// given, and fails if any status sampled on the way is Unavailable.
async fn follow_manager_status(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
    expected_message: Option<&str>,
) -> ModelDeployment {
    let mut sampled = Vec::new();
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let deployment = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model status")
                .into_inner()
                .deployments
                .into_iter()
                .find(|deployment| deployment.model_deployment_id == deployment_id)
                .expect("manager lists the deployment");
            let state = deployment.state.as_ref().expect("manager deployment state");
            let status = ModelDeploymentStatus::try_from(state.status).expect("known status");
            let sample = (status, state.status_message.clone());
            if sampled.last() != Some(&sample) {
                sampled.push(sample);
            }
            assert_ne!(
                status,
                ModelDeploymentStatus::Unavailable,
                "a connected worker must not show Unavailable on the way to {expected:?}: {sampled:?}"
            );
            if status == expected
                && expected_message.is_none_or(|message| message == state.status_message)
            {
                return deployment;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager model status deadline")
}

async fn wait_for_manager_absence(
    models: &mut ModelDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let deployments = models
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("manager model list after teardown")
                .into_inner()
                .deployments;
            if deployments
                .iter()
                .all(|deployment| deployment.model_deployment_id != deployment_id)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager teardown finalization deadline");
}

async fn wait_for_worker_cleanup_retirement(state_path: std::path::PathBuf, deployment_id: &str) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            if fs::read(&state_path)
                .ok()
                .and_then(|bytes| NodeCurrentState::decode(bytes.as_slice()).ok())
                .is_some_and(|state| {
                    state
                        .model_deployments
                        .iter()
                        .all(|record| record.model_deployment_id != deployment_id)
                        && state
                            .model_cleanup_evidence
                            .iter()
                            .all(|evidence| evidence.model_deployment_id != deployment_id)
                })
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker cleanup receipt deadline");
}

async fn wait_for_worker_status(
    state_path: std::path::PathBuf,
    deployment_id: &str,
    expected: ModelDeploymentStatus,
) -> bool {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            if fs::read(&state_path)
                .ok()
                .and_then(|bytes| NodeCurrentState::decode(bytes.as_slice()).ok())
                .and_then(|state| {
                    state
                        .model_deployments
                        .into_iter()
                        .find(|record| record.model_deployment_id == deployment_id)
                })
                .and_then(|record| record.state)
                .is_some_and(|state| state.status == expected as i32)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .is_ok()
}

fn successful_runtime_operations(records: &[serde_json::Value], operation: &str) -> usize {
    records
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "model_runtime_outcome"
                && record["fields"]["operation"] == operation
                && record["fields"]["outcome"] == "success"
        })
        .count()
}
