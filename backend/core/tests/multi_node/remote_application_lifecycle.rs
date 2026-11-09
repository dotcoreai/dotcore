//! Proves the manager controls an application deployment that only an independently provisioned
//! worker process realizes, through create, pause, resume, restarts of either process, a worker
//! disconnect, logs including retained preparation steps, linked-route renames, and teardown.
//!
//! The harness uses generated worker custody and real mutual TLS. The worker's simulated runtime
//! publishes every application group on a port where the test serves readiness, so the worker
//! itself probes the endpoint the manager projects. Status waits never depend on the observation
//! cadence: a stale projection is asserted only after the manager's fixed liveness window expires,
//! and every other wait polls until the expected state. Durable per-process events prove the
//! worker performed every runtime action, including storage removal, and the manager none.

use std::{
    fs,
    net::SocketAddr,
    time::{Duration, Instant},
};

use dotcore::testing::{MISSED_PUBLICATIONS_BEFORE_REOPEN, Timing};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationDeploymentDesiredState, ApplicationLifecycleState,
        CreateApplicationDeploymentRequest, DeploymentLogHistoryState, DeploymentLogResumePosition,
        DeploymentLogSourceState, FollowDeploymentLogsRequest, GetApplicationDeploymentRequest,
        ListApplicationDeploymentsRequest, ListApplicationDeploymentsResponse,
        PauseDeploymentRequest, ReadDeploymentLogsRequest, ReadDeploymentLogsResponse,
        RemoveApplicationDeploymentRequest, ResumeDeploymentRequest,
        application_deployment_service_client::ApplicationDeploymentServiceClient,
        follow_deployment_logs_response,
    },
    deployment::v1::ExactPlacement,
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection, InferenceRoute,
        ReplaceRouteRequest, route_service_client::RouteServiceClient,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest,
        create_model_deployment_request,
        model_deployment_service_client::ModelDeploymentServiceClient,
    },
    site::v1::{ListMachinesRequest, NodeObservationState, Site, execution_target},
    watch::v1::{
        KeyedResource, ResourceSubscription, WatchResourcesRequest, WatchResourcesResponse,
        keyed_resource::Payload, resource_subscription,
        resource_watch_service_client::ResourceWatchServiceClient, watch_resources_response,
    },
};
use support::{ProcessTestControls, TwoProcessObservationHarness};
use tokio::io::AsyncWriteExt;

use crate::support;

const MILESTONE_DEADLINE: Duration = Duration::from_secs(15);
/// Longest wait for the next resource watch event, far above the one second tick.
const WATCH_EVENT_DEADLINE: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const WORKER_NODE_ID: &str = support::WORKER_NODE_ID;
/// Synthetic site catalog application the manager deploys on the worker.
const APPLICATION_ID: &str = "remote-fixture";
/// Application whose service comes from the development source and is pinned by the worker.
const NIX_APPLICATION_ID: &str = "remote-nix-fixture";
const NIX_APPLICATION_PACKAGE: &str = "apps.remote-nix-fixture.docker";
/// The synthetic application's only service, which is also its log source.
const SERVICE_NAME: &str = "web";
const ROUTE_NAME: &str = "worker-application-route";
const RENAMED_ROUTE_NAME: &str = "worker-application-route-renamed";
/// Node silence long enough that only a closed stream explains a prompt disconnect.
const LONG_NODE_SILENCE: Duration = Duration::from_secs(60);
/// Republish interval near the longest a manager accepts, leaving the processes time to start
/// and the test time to change the worker's view before any republication.
const LONG_REPUBLISH_INTERVAL: Duration = Duration::from_secs(25);

#[tokio::test]
async fn manager_controls_a_worker_hosted_application_across_restarts_and_disconnects() {
    let Some(health) = WorkerHealthServer::start().await else {
        return;
    };
    let Some(mut harness) = start_harness(&health, true) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let placement = node_application_placement(&harness);
    let mut applications = application_client(manager_address).await;

    // Create without a linked route: only the worker realizes it, and the manager projects the
    // worker-published endpoint.
    let created = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(placement.clone()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a worker application target")
        .into_inner()
        .deployment
        .expect("created application");
    let deployment_id = created.application_deployment_id.clone();
    assert_eq!(created.placement.as_ref(), Some(&placement));
    assert_eq!(
        created.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    let ready = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&ready, &placement, &health);
    assert_eq!(ready.selected_inference_route, None);
    assert!(
        worker_realizes(&harness, &deployment_id),
        "the worker retains ownership evidence before realizing the deployment"
    );

    // Logs come from the worker's exact group, read and followed through the manager.
    let newest = read_logs(&mut applications, &deployment_id, None)
        .await
        .expect("manager reads worker-hosted application logs");
    let generation = simulated_log_generation(&newest);
    assert_simulated_log_page(&newest, &deployment_id, generation, 8..=12);
    assert_eq!(
        newest.history_state,
        DeploymentLogHistoryState::OlderOutputMayRemain as i32
    );
    let older = read_logs(
        &mut applications,
        &deployment_id,
        newest.next_before_timestamp,
    )
    .await
    .expect("manager reads an older worker-hosted page");
    assert_simulated_log_page(&older, &deployment_id, generation, 3..=7);
    assert_eq!(
        follow_log_contents(&mut applications, &deployment_id, 10, 2).await,
        [11, 12]
            .map(|line| simulated_log_line(&deployment_id, generation, line))
            .to_vec(),
        "the follow resumes after line 10 and forwards the worker's lines"
    );

    // Pause keeps the retained group, whose logs stay readable; resume restarts it.
    applications
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts pause");
    wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    let paused_logs = read_logs(&mut applications, &deployment_id, None)
        .await
        .expect("manager reads Paused worker-hosted logs");
    assert_simulated_log_page(&paused_logs, &deployment_id, generation, 8..=12);

    // A preparation step the worker retains reads and follows through the manager, ahead of the
    // service source.
    let step_source = format!("prepare:{SERVICE_NAME}:build");
    dotcore::testing::append_preparation_log(
        &harness.worker().site_root().join("node"),
        &deployment_id,
        &step_source,
        // Older than every simulated workload line, so newest pages below stay unchanged.
        std::time::UNIX_EPOCH + Duration::from_secs(1_600_000_000),
        "building service",
    );
    let all_sources = read_logs(&mut applications, &deployment_id, None)
        .await
        .expect("manager reads every worker-hosted source");
    assert_eq!(
        all_sources
            .sources
            .iter()
            .map(|source| (source.source_name.clone(), source.source_label.clone()))
            .collect::<Vec<_>>(),
        [
            (step_source.clone(), format!("{SERVICE_NAME} build")),
            (SERVICE_NAME.to_owned(), SERVICE_NAME.to_owned())
        ]
    );
    let step = applications
        .read_deployment_logs(ReadDeploymentLogsRequest {
            application_deployment_id: deployment_id.clone(),
            source_names: vec![step_source.clone()],
            page_size: 5,
            ..Default::default()
        })
        .await
        .expect("manager reads a worker-hosted step")
        .into_inner();
    assert_eq!(
        step.entries
            .iter()
            .map(|entry| entry.content.as_slice())
            .collect::<Vec<_>>(),
        [b"building service".as_slice()]
    );
    assert_eq!(
        follow_step_contents(&mut applications, &deployment_id, &step_source, 1).await,
        [b"building service".to_vec()],
        "the manager follows the worker's step"
    );

    resume(&mut applications, &deployment_id).await;
    let resumed = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&resumed, &placement, &health);

    // A restarted manager rebuilds the worker's view from its declarations; the worker keeps the
    // running group.
    harness.manager_mut().restart();
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    let mut applications = application_client(manager_address).await;
    let after_manager_restart = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&after_manager_restart, &placement, &health);

    // A restarted worker recovers the deployment from the manager's view and its own evidence.
    // The manager admits the new process once the killed one's control stream closed, and the
    // simulated runtime lost the group with the killed process, so the new process creates it.
    harness.worker_mut().restart();
    wait_for_worker_runtime_events(&harness, &deployment_id, "application_runtime_create", 2).await;
    let after_worker_restart = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&after_worker_restart, &placement, &health);
    assert!(worker_realizes(&harness, &deployment_id));

    // A frozen worker stops all contact. Once its liveness expires, the manager keeps the last
    // observation and endpoint, marked stale, and reports its log source unavailable.
    harness.worker_mut().pause();
    harness.expire_silent_worker().await;
    support::wait_for_worker_observation(manager_address, NodeObservationState::Disconnected).await;
    let stale = get(&mut applications, &deployment_id).await;
    let stale_state = stale.state.as_ref().expect("stale application state");
    assert!(stale_state.observation_stale);
    assert_eq!(
        stale_state.lifecycle_state,
        ApplicationLifecycleState::Ready as i32
    );
    assert_eq!(
        stale_state
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.url.as_str()),
        Some(health.url().as_str()),
        "the stale projection keeps the direct worker endpoint"
    );
    let disconnected_logs = read_logs(&mut applications, &deployment_id, None)
        .await
        .expect("manager answers a log read while the worker is disconnected");
    assert!(disconnected_logs.entries.is_empty());
    assert_eq!(
        log_sources(&disconnected_logs),
        [(SERVICE_NAME, DeploymentLogSourceState::Unavailable as i32)]
    );

    // Pause while disconnected stays pending until the worker reports again.
    applications
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts pause while the worker is disconnected");
    let pending = get(&mut applications, &deployment_id).await;
    assert_eq!(
        pending.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    let pending_state = pending.state.as_ref().expect("pending pause state");
    assert!(pending_state.observation_stale);
    assert_eq!(
        pending_state.lifecycle_state,
        ApplicationLifecycleState::Pausing as i32,
        "no worker confirmed the pause"
    );
    harness.worker_mut().resume();
    wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    resume(&mut applications, &deployment_id).await;
    wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;

    // Teardown removes the group and the deployment's storage on the worker, then the manager
    // finalizes on the worker's exact cleanup evidence.
    applications
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_absence(&mut applications, &deployment_id).await;
    wait_for_worker_cleanup_retirement(&harness, &deployment_id).await;
    assert!(
        !harness
            .worker()
            .site_root()
            .join("node/preparation-logs/application")
            .join(&deployment_id)
            .exists(),
        "the worker removes the deployment's preparation logs with its cleanup"
    );
    assert_eq!(
        read_logs(&mut applications, &deployment_id, None)
            .await
            .expect_err("a torn-down deployment has no logs")
            .code(),
        tonic::Code::NotFound
    );

    harness.worker_mut().shutdown();
    harness.manager_mut().shutdown();
    let worker_records = support::retained_json_records(harness.worker().site_root());
    assert_eq!(
        runtime_events(
            &worker_records,
            &deployment_id,
            "application_runtime_create"
        ),
        ["created", "created"],
        "the worker creates the group once, and again only after its own restart lost it"
    );
    assert_eq!(
        runtime_events(
            &worker_records,
            &deployment_id,
            "application_runtime_storage_removal"
        ),
        ["absent"],
        "teardown deletes the deployment's storage on the worker"
    );
    assert!(
        launched_route_names(&worker_records, &deployment_id).is_empty(),
        "an application without a linked route projects no route name"
    );
    let manager_records = support::retained_json_records(harness.manager().site_root());
    assert!(
        manager_records.iter().all(|record| {
            !record["fields"]["event_kind"]
                .as_str()
                .is_some_and(|kind| kind.starts_with("application_runtime_"))
                || record["fields"]["deployment_id"] != deployment_id.as_str()
        }),
        "the manager makes no runtime call for a worker-hosted deployment"
    );
    assert!(
        !fs::read_to_string(harness.manager().site_root().join("site.yaml"))
            .expect("manager site.yaml")
            .contains(&deployment_id),
        "the manager durably removes the declaration"
    );
    assert!(
        support::retained_state(harness.manager().state_path())
            .manager_application_lifecycle_records
            .iter()
            .all(|record| record.deployment_id != deployment_id),
        "the manager prunes the lifecycle record once the removal is durable"
    );
}

#[tokio::test]
async fn a_worker_hosted_application_relaunches_when_its_linked_route_is_renamed() {
    if !support::nvidia_driver_observed() {
        return;
    }
    let Some(health) = WorkerHealthServer::start().await else {
        return;
    };
    let Some(mut harness) = start_harness(&health, false) else {
        return;
    };
    let manager_address = harness
        .manager()
        .addresses()
        .management
        .expect("management listener");
    support::wait_for_worker_connection(manager_address).await;
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    let placement = node_application_placement(&harness);
    let route = create_manager_route(manager_address).await;
    let mut applications = application_client(manager_address).await;

    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(placement.clone()),
            inference_route_id: Some(route.inference_route_id.clone()),
            expected_inference_route_generation: route.generation,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a route-linked worker application")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id;
    let linked =
        wait_for_route_launch(&mut applications, &harness, &deployment_id, ROUTE_NAME).await;
    assert_ready(&linked, &placement, &health);

    // Renaming the linked route republishes the worker's assignment, and the worker relaunches
    // the group with the new name.
    RouteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager route API connection")
        .replace_route(ReplaceRouteRequest {
            inference_route_id: route.inference_route_id.clone(),
            expected_generation: route.generation,
            name: RENAMED_ROUTE_NAME.to_owned(),
            enabled: true,
            fallback_groups: route.fallback_groups.clone(),
        })
        .await
        .expect("manager renames the linked route");
    let renamed = wait_for_route_launch(
        &mut applications,
        &harness,
        &deployment_id,
        RENAMED_ROUTE_NAME,
    )
    .await;
    assert_ready(&renamed, &placement, &health);

    applications
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_absence(&mut applications, &deployment_id).await;
    wait_for_worker_cleanup_retirement(&harness, &deployment_id).await;

    harness.worker_mut().shutdown();
    harness.manager_mut().shutdown();
    let worker_records = support::retained_json_records(harness.worker().site_root());
    assert_eq!(
        launched_route_names(&worker_records, &deployment_id),
        [ROUTE_NAME, RENAMED_ROUTE_NAME],
        "the worker launches once per projected route name"
    );
    assert_eq!(
        runtime_events(
            &worker_records,
            &deployment_id,
            "application_runtime_remove"
        ),
        ["removed_observed"],
        "the rename replaces the group launched with the old name"
    );
    assert_eq!(
        runtime_events(
            &worker_records,
            &deployment_id,
            "application_runtime_create"
        ),
        ["created", "created"]
    );
    assert_eq!(
        runtime_events(
            &worker_records,
            &deployment_id,
            "application_runtime_storage_removal"
        ),
        ["absent"],
        "the relaunch keeps the deployment's storage; only teardown deletes it"
    );
}

#[tokio::test]
async fn a_split_network_worker_application_is_reached_at_the_declared_worker_address() {
    let Some(health) = WorkerHealthServer::start().await else {
        return;
    };
    let Some(harness) = TwoProcessObservationHarness::start_split(
        ProcessTestControls {
            site_catalog: APPLICATION_CATALOG,
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            application_endpoint_port: Some(health.address.port()),
            ..ProcessTestControls::default()
        },
        &[health.address.port()],
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
    let placement = node_application_placement(&harness);
    let mut applications = application_client(manager_address).await;

    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(placement.clone()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a worker application target")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id;

    // The application calls inference at the manager address its hosting node reaches.
    let manager_inference = harness
        .manager()
        .addresses()
        .inference
        .expect("inference listener");
    let declaration = harness
        .manager_site()
        .application_deployments
        .into_iter()
        .find(|declaration| declaration.application_deployment_id == deployment_id)
        .expect("application declaration");
    let base_url = declaration
        .configuration_attachments
        .iter()
        .find(|attachment| attachment.binding_name == "inference-base-url")
        .map(|attachment| attachment.value.as_str());
    assert_eq!(
        base_url,
        Some(
            format!(
                "http://{}:{}/v1",
                support::SPLIT_MANAGER_HOST,
                manager_inference.port()
            )
            .as_str()
        )
    );

    // The worker publishes on its bind address; the manager projects the declared address, which
    // the network forwards back to the worker.
    let ready = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    let declared_endpoint = SocketAddr::from((support::SPLIT_WORKER_HOST, health.address.port()));
    assert_eq!(
        ready
            .state
            .as_ref()
            .and_then(|state| state.endpoint.as_ref())
            .map(|endpoint| endpoint.url.as_str()),
        Some(format!("http://{declared_endpoint}").as_str())
    );
    assert_eq!(http_status_line(declared_endpoint).await, "HTTP/1.1 200 OK");
    let logs = read_logs(&mut applications, &deployment_id, None)
        .await
        .expect("manager reads worker-hosted logs across the split network");
    assert_simulated_log_page(
        &logs,
        &deployment_id,
        simulated_log_generation(&logs),
        8..=12,
    );

    applications
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("manager accepts teardown");
    wait_for_absence(&mut applications, &deployment_id).await;
    wait_for_worker_cleanup_retirement(&harness, &deployment_id).await;
}

#[tokio::test]
async fn a_worker_follows_pushed_views_and_its_closed_stream_disconnects_it_at_once() {
    let Some(health) = WorkerHealthServer::start().await else {
        return;
    };
    // Taken before either process starts, so no republication reaches the worker before this
    // instant plus the republish interval: the first publication answers the worker's stream
    // open, and each republication follows the previous publication by a whole interval.
    let started = Instant::now();
    // Both processes run one policy with a long republish interval and relations that pass the
    // timing check: the worker neither reopens its stream nor loses its action lease between
    // republications.
    let timing = {
        let mut timing = Timing::test();
        timing.node_control.node_silence = LONG_NODE_SILENCE;
        timing.node_control.view_republish_interval = LONG_REPUBLISH_INTERVAL;
        timing.node_control.manager_silence =
            LONG_REPUBLISH_INTERVAL * MISSED_PUBLICATIONS_BEFORE_REOPEN;
        timing.node_control.action_lease =
            LONG_REPUBLISH_INTERVAL * (MISSED_PUBLICATIONS_BEFORE_REOPEN + 1);
        timing.check().expect("a valid long-republish policy");
        timing
    };
    let Some(mut harness) = TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: APPLICATION_CATALOG,
            timing,
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            application_endpoint_port: Some(health.address.port()),
            timing,
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
    let placement = node_application_placement(&harness);
    let mut applications = application_client(manager_address).await;

    // The changed view reaches the worker before any republication could have carried it.
    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(placement.clone()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a worker application target")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id;
    let worker_root = harness.worker().site_root().to_owned();
    let pushed = tokio::time::timeout_at((started + LONG_REPUBLISH_INTERVAL).into(), async {
        while runtime_events(
            &support::retained_json_records(&worker_root),
            &deployment_id,
            "application_runtime_create",
        )
        .is_empty()
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    assert!(
        pushed.is_ok(),
        "the worker realizes the new deployment before the first republication"
    );

    // The worker's status reaches the manager over its stream.
    let ready = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&ready, &placement, &health);

    // Only the closed stream can disconnect the killed worker this early in the silence bound.
    let killed = Instant::now();
    harness.worker_mut().stop();
    support::wait_for_worker_observation(manager_address, NodeObservationState::Disconnected).await;
    let disconnected_after = killed.elapsed();
    assert!(
        disconnected_after < LONG_NODE_SILENCE / 2,
        "the killed worker disconnected {disconnected_after:?} after it stopped"
    );

    // The old incarnation's stream closed, so the restarted process is admitted at once and
    // realizes the deployment again.
    harness.worker_mut().restart();
    support::wait_for_worker_connection(manager_address).await;
    wait_for_worker_runtime_events(&harness, &deployment_id, "application_runtime_create", 2).await;
    let after_restart = wait_for_current(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_ready(&after_restart, &placement, &health);
    let admitted_after = killed.elapsed();
    assert!(
        admitted_after < LONG_NODE_SILENCE,
        "the restarted worker was admitted {admitted_after:?} after the old one stopped"
    );
}

#[tokio::test]
async fn the_resource_watch_shows_a_worker_deployment_status_and_its_closed_stream_disconnection() {
    let Some(health) = WorkerHealthServer::start().await else {
        return;
    };
    let Some(mut harness) = TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: APPLICATION_CATALOG,
            timing: {
                let mut timing = Timing::test();
                timing.node_control.node_silence = LONG_NODE_SILENCE;
                timing
            },
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            application_endpoint_port: Some(health.address.port()),
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
    let placement = node_application_placement(&harness);
    let mut applications = application_client(manager_address).await;
    let mut watch = ResourceWatchServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager resource watch connection")
        .watch_resources(WatchResourcesRequest {
            subscriptions: vec![
                ResourceSubscription {
                    key: "machines".to_owned(),
                    kind: Some(resource_subscription::Kind::Machines(
                        ListMachinesRequest {},
                    )),
                },
                ResourceSubscription {
                    key: "applications".to_owned(),
                    kind: Some(resource_subscription::Kind::ApplicationDeployments(
                        ListApplicationDeploymentsRequest::default(),
                    )),
                },
            ],
        })
        .await
        .expect("open resource watch")
        .into_inner();

    // The first round shows the connected worker and no deployment, then completes.
    let mut first_round = Vec::new();
    let manager_session_id = loop {
        let response = next_watch_event(&mut watch).await;
        match response.event.expect("watch event") {
            watch_resources_response::Event::Resource(resource) => first_round.push(resource),
            watch_resources_response::Event::SnapshotComplete(_) => {
                break response.manager_session_id;
            }
            watch_resources_response::Event::Heartbeat(_) => panic!("heartbeat before completion"),
        }
    };
    assert_eq!(manager_session_id.len(), 32);
    first_round.sort_by(|left, right| left.key.cmp(&right.key));
    let [applications_round, machines_round] = first_round.as_slice() else {
        panic!("one payload per key in the first round: {first_round:?}");
    };
    assert_eq!(applications_round.key, "applications");
    assert_eq!(
        applications_round.payload,
        Some(Payload::ApplicationDeployments(
            ListApplicationDeploymentsResponse::default()
        ))
    );
    assert_eq!(machines_round.key, "machines");
    assert_eq!(
        worker_state(machines_round),
        Some(NodeObservationState::Connected as i32)
    );

    // The worker's status change arrives as an update of the applications key. The first round is
    // read on open, so a machine observation received just after it can follow as an update.
    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: APPLICATION_ID.to_owned(),
            placement: Some(placement.clone()),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a worker application target")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id;
    let ready = loop {
        let response = next_watch_event(&mut watch).await;
        assert_eq!(response.manager_session_id, manager_session_id);
        let Some(watch_resources_response::Event::Resource(resource)) = response.event else {
            continue;
        };
        if resource.key == "machines" {
            continue;
        }
        let Some(Payload::ApplicationDeployments(list)) = resource.payload else {
            panic!(
                "only the applications key carries deployments: {:?}",
                resource.key
            );
        };
        assert_eq!(resource.key, "applications");
        let deployment = list
            .deployments
            .into_iter()
            .find(|deployment| deployment.application_deployment_id == deployment_id);
        if let Some(deployment) = deployment.filter(|deployment| {
            deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                    && !state.observation_stale
            })
        }) {
            break deployment;
        }
    };
    assert_ready(&ready, &placement, &health);

    // Only the closed stream can disconnect the killed worker this early in the silence bound.
    let killed = Instant::now();
    harness.worker_mut().stop();
    loop {
        let response = next_watch_event(&mut watch).await;
        let Some(watch_resources_response::Event::Resource(resource)) = response.event else {
            continue;
        };
        if resource.key == "machines"
            && worker_state(&resource) == Some(NodeObservationState::Disconnected as i32)
        {
            break;
        }
    }
    let disconnected_after = killed.elapsed();
    assert!(
        disconnected_after < LONG_NODE_SILENCE / 2,
        "the watch showed the disconnection {disconnected_after:?} after the worker stopped"
    );
}

#[tokio::test]
async fn a_worker_pins_a_nix_application_on_its_simulated_target_and_reports_the_pin() {
    if !support::worker_loopback_address_available() {
        return;
    }
    let Some(harness) = TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: APPLICATION_CATALOG,
            ..ProcessTestControls::default()
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
    let placement = node_application_placement(&harness);
    let mut applications = application_client(manager_address).await;

    let deployment_id = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: NIX_APPLICATION_ID.to_owned(),
            placement: Some(placement),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("manager accepts a Nix application on the worker")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id;

    // No server answers on the worker, so readiness comes from the simulated runtime alone.
    // The manager records the pin after it records the state, so the pin is awaited too.
    let ready = wait_for(&mut applications, &deployment_id, |deployment| {
        !deployment.node_resolved_launch_identities.is_empty()
            && deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                    && !state.observation_stale
            })
    })
    .await;
    assert_eq!(
        ready
            .state
            .as_ref()
            .and_then(|state| state.failure.as_ref()),
        None
    );
    // The simulated target pins a synthetic loader path without evaluating the package.
    let [pin] = ready.node_resolved_launch_identities.as_slice() else {
        panic!(
            "the manager records the pin the worker resolved: {:?}",
            ready.node_resolved_launch_identities
        );
    };
    let reference = dotcore_domain::nix::docker_launch_reference(&pin.launch_identity)
        .expect("a loader store path");
    assert_eq!(pin.selection_key, "web");
    assert!(
        reference.starts_with(&format!(
            "dotcore-nix/docker-image-simulated-{}:",
            NIX_APPLICATION_PACKAGE.replace('.', "-")
        )),
        "{reference}"
    );
}

/// Observation state of the worker in a machines payload.
fn worker_state(resource: &KeyedResource) -> Option<i32> {
    let Some(Payload::Machines(machines)) = &resource.payload else {
        panic!("machines payload: {resource:?}");
    };
    machines
        .machines
        .iter()
        .find(|machine| machine.machine_id == WORKER_NODE_ID)
        .and_then(|machine| machine.observation_freshness.as_ref())
        .map(|freshness| freshness.state)
}

/// Next event of a resource watch; ticks are one second, so the deadline only bounds a hang.
async fn next_watch_event(
    watch: &mut tonic::Streaming<WatchResourcesResponse>,
) -> WatchResourcesResponse {
    tokio::time::timeout(WATCH_EVENT_DEADLINE, watch.message())
        .await
        .expect("resource watch event deadline")
        .expect("resource watch status")
        .expect("resource watch ended")
}

/// Synthetic applications. The first has one digest-pinned service with a readiness check,
/// retained storage, the site inference base URL, and an optional route-name link with an empty
/// literal fallback; the second has one service from the development source.
const APPLICATION_CATALOG: &[(&str, &str)] = &[
    (
        "applications/remote-fixture.yaml",
        r#"applicationId: "remote-fixture"
displayName: "Remote fixture"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        oci:
          repository:
            registry: { host: "registry.dotcore.test" }
            name: "remote-fixture"
          digest:
            algorithm: "sha256"
            encoded: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        supportedPlatforms:
          - { operatingSystem: "linux", architecture: "amd64" }
          - { operatingSystem: "linux", architecture: "arm64" }
      ports: [{ portName: "http", portNumber: 8080 }]
      configurationBindings:
        - bindingName: "inference-base-url"
          environmentVariable: "INFERENCE_BASE_URL"
          resolvedValueName: "configured-inference-openai-base-url"
        - bindingName: "route-name"
          environmentVariable: "ROUTE_NAME"
          literalValue: ""
          input:
            inferenceRoute:
              projection: "APPLICATION_INFERENCE_ROUTE_PROJECTION_ROUTE_NAME"
      storageBindings: [{ bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_DATA", scope: "STORAGE_SCOPE_DEPLOYMENT" }]
      readiness: { httpGet: { portName: "http", path: "/health" } }
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
    ),
    (
        "applications/remote-nix-fixture.yaml",
        r#"applicationId: "remote-nix-fixture"
displayName: "Remote Nix fixture"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        nix:
          flake: "self"
          package: "apps.remote-nix-fixture.docker"
          dockerImage: {}
        supportedPlatforms:
          - { operatingSystem: "linux", architecture: "amd64" }
          - { operatingSystem: "linux", architecture: "arm64" }
      ports: [{ portName: "http", portNumber: 8080 }]
      readiness: { httpGet: { portName: "http", path: "/health" } }
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#,
    ),
];

/// Readiness endpoint on the worker's publication host, answering every request with 200.
struct WorkerHealthServer {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
    /// Held because a split-network forwarder binds the same port on another loopback address.
    _port: dotcore_domain::test_support::ports::ReservedPort,
}

impl WorkerHealthServer {
    /// Binds the worker's loopback host, or returns `None` when the host cannot bind it.
    async fn start() -> Option<Self> {
        if !support::worker_loopback_address_available() {
            return None;
        }
        let port = dotcore_domain::test_support::ports::reserve_port();
        let listener = tokio::net::TcpListener::bind((support::WORKER_LOOPBACK_HOST, port.port()))
            .await
            .expect("worker readiness listener");
        let address = listener.local_addr().expect("worker readiness address");
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    support::read_http_request(&mut stream).await;
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        Some(Self {
            address,
            task,
            _port: port,
        })
    }

    /// The direct endpoint URL the worker publishes and the manager projects.
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for WorkerHealthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Starts the manager with the synthetic application catalog, on a manual clock when asked, and
/// the worker publishing every application group on the readiness server's port.
fn start_harness(
    health: &WorkerHealthServer,
    manual_clock: bool,
) -> Option<TwoProcessObservationHarness> {
    TwoProcessObservationHarness::start_with_controls(
        ProcessTestControls {
            site_catalog: APPLICATION_CATALOG,
            manual_clock,
            ..ProcessTestControls::default()
        },
        ProcessTestControls {
            application_endpoint_port: Some(health.address.port()),
            ..ProcessTestControls::default()
        },
    )
}

/// Returns the exact placement on the application target provisioned for the worker.
fn node_application_placement(harness: &TwoProcessObservationHarness) -> ExactPlacement {
    let site: Site = dotcore_proto::yaml::from_str(
        &fs::read_to_string(harness.manager().site_root().join("site.yaml"))
            .expect("manager site.yaml"),
    )
    .expect("manager site");
    let target = site
        .execution_targets
        .iter()
        .find(|target| {
            target.site_node_id == WORKER_NODE_ID
                && matches!(
                    target.runtime_binding,
                    Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                )
        })
        .expect("provisioned worker application target");
    support::exact_placement(WORKER_NODE_ID, &target.execution_target_id)
}

/// Creates a manager-hosted model deployment and an enabled route whose only member it is.
async fn create_manager_route(manager_address: SocketAddr) -> InferenceRoute {
    let model_deployment_id =
        ModelDeploymentServiceClient::connect(format!("http://{manager_address}"))
            .await
            .expect("manager model API connection")
            .create_model_deployment(CreateModelDeploymentRequest {
                source: Some(create_model_deployment_request::Source::Local(
                    CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                )),
                exposure: Some(support::hidden_model_exposure()),
                model_deployment_name: None,
            })
            .await
            .expect("manager accepts the route's model deployment")
            .into_inner()
            .deployment
            .expect("accepted model deployment")
            .model_deployment_id;
    RouteServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager route API connection")
        .create_route(CreateRouteRequest {
            name: ROUTE_NAME.to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::Single as i32,
                model_deployment_ids: vec![model_deployment_id],
            }],
        })
        .await
        .expect("manager accepts the route")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("created route")
}

/// Sends one HTTP GET to `address` and returns the response status line.
async fn http_status_line(address: SocketAddr) -> String {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to the application endpoint");
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: application\r\nConnection: close\r\n\r\n")
        .await
        .expect("send the request");
    let mut response = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response)
        .await
        .expect("read the response");
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

async fn application_client(
    manager_address: SocketAddr,
) -> ApplicationDeploymentServiceClient<tonic::transport::Channel> {
    ApplicationDeploymentServiceClient::connect(format!("http://{manager_address}"))
        .await
        .expect("manager application API connection")
}

/// Asserts a current Ready projection of the worker-realized deployment and its endpoint.
fn assert_ready(
    deployment: &ApplicationDeployment,
    placement: &ExactPlacement,
    health: &WorkerHealthServer,
) {
    let state = deployment.state.as_ref().expect("Ready application state");
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Ready as i32
    );
    assert!(!state.observation_stale);
    assert_eq!(state.failure, None);
    assert_eq!(state.observed_placement.as_ref(), Some(placement));
    assert_eq!(
        state
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.url.as_str()),
        Some(health.url().as_str()),
        "the manager projects the worker-published direct endpoint"
    );
}

async fn resume(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
) {
    applications
        .resume_deployment(ResumeDeploymentRequest {
            application_deployment_id: deployment_id.to_owned(),
        })
        .await
        .expect("manager accepts resume");
}

async fn get(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
) -> ApplicationDeployment {
    applications
        .get_application_deployment(GetApplicationDeploymentRequest {
            application_deployment_id: deployment_id.to_owned(),
        })
        .await
        .expect("manager application status")
        .into_inner()
        .deployment
        .expect("listed application")
}

/// Waits until the manager projects `expected` from current, not stale, worker status.
async fn wait_for_current(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    expected: ApplicationLifecycleState,
) -> ApplicationDeployment {
    wait_for(applications, deployment_id, |deployment| {
        deployment.state.as_ref().is_some_and(|state| {
            state.lifecycle_state == expected as i32 && !state.observation_stale
        })
    })
    .await
}

/// Waits until the deployment is Ready with `route_name` projected and the worker has launched it
/// with that name.
async fn wait_for_route_launch(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    harness: &TwoProcessObservationHarness,
    deployment_id: &str,
    route_name: &str,
) -> ApplicationDeployment {
    let worker_root = harness.worker().site_root().to_owned();
    wait_for(applications, deployment_id, |deployment| {
        deployment
            .selected_inference_route
            .as_ref()
            .is_some_and(|route| route.route_name == route_name)
            && deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                    && !state.observation_stale
            })
            && launched_route_names(&support::retained_json_records(&worker_root), deployment_id)
                .last()
                .is_some_and(|launched| *launched == route_name)
    })
    .await
}

async fn wait_for(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    matches: impl Fn(&ApplicationDeployment) -> bool,
) -> ApplicationDeployment {
    let mut last = None;
    let result = tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let deployment = get(applications, deployment_id).await;
            if matches(&deployment) {
                return deployment;
            }
            last = Some(deployment);
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("manager application status deadline; last: {last:?}"))
}

async fn wait_for_absence(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
) {
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let status = applications
                .get_application_deployment(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                })
                .await;
            if status.is_err_and(|status| status.code() == tonic::Code::NotFound) {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("manager teardown finalization deadline");
}

/// Waits until the worker has logged `count` events of one runtime kind for the deployment,
/// allowing a liveness expiry before a restarted worker is admitted.
async fn wait_for_worker_runtime_events(
    harness: &TwoProcessObservationHarness,
    deployment_id: &str,
    event_kind: &str,
    count: usize,
) {
    let worker_root = harness.worker().site_root();
    tokio::time::timeout(support::LIVENESS_EXPIRY_DEADLINE, async {
        while runtime_events(
            &support::retained_json_records(worker_root),
            deployment_id,
            event_kind,
        )
        .len()
            < count
        {
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker runtime event deadline");
}

/// Whether the worker retains ownership evidence for the deployment's realization.
fn worker_realizes(harness: &TwoProcessObservationHarness, deployment_id: &str) -> bool {
    support::retained_node_state(harness.worker().node_state_path())
        .application_realizations
        .iter()
        .any(|record| record.application_deployment_id == deployment_id)
}

/// Waits until the worker has dropped the deployment's realization and retired its cleanup
/// evidence on the manager's receipt.
async fn wait_for_worker_cleanup_retirement(
    harness: &TwoProcessObservationHarness,
    deployment_id: &str,
) {
    let state_path = harness.worker().node_state_path();
    tokio::time::timeout(MILESTONE_DEADLINE, async {
        loop {
            let state = support::retained_node_state(state_path.clone());
            if state
                .application_realizations
                .iter()
                .all(|record| record.application_deployment_id != deployment_id)
                && state
                    .application_cleanup_evidence
                    .iter()
                    .all(|evidence| evidence.application_deployment_id != deployment_id)
            {
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    })
    .await
    .expect("worker cleanup receipt deadline");
}

/// Reads the newest five log entries of a deployment through the manager, before an optional
/// whole-second boundary.
async fn read_logs(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    before_timestamp: Option<dotcore_proto::google::protobuf::Timestamp>,
) -> Result<ReadDeploymentLogsResponse, tonic::Status> {
    applications
        .read_deployment_logs(ReadDeploymentLogsRequest {
            application_deployment_id: deployment_id.to_owned(),
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
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    after: u64,
    count: usize,
) -> Vec<Vec<u8>> {
    let mut follow = applications
        .follow_deployment_logs(FollowDeploymentLogsRequest {
            application_deployment_id: deployment_id.to_owned(),
            resume_positions: vec![DeploymentLogResumePosition {
                source_name: SERVICE_NAME.to_owned(),
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
                (SERVICE_NAME, DeploymentLogSourceState::Available as i32)
            ),
            follow_deployment_logs_response::Event::Entry(entry) => {
                assert_eq!(entry.source_name, SERVICE_NAME);
                contents.push(entry.content);
            }
        }
    }
    contents
}

/// Follows one preparation step of a deployment through the manager from its first entry,
/// returning the first `count` entries, then drops the follow.
async fn follow_step_contents(
    applications: &mut ApplicationDeploymentServiceClient<tonic::transport::Channel>,
    deployment_id: &str,
    source_name: &str,
    count: usize,
) -> Vec<Vec<u8>> {
    let mut follow = applications
        .follow_deployment_logs(FollowDeploymentLogsRequest {
            application_deployment_id: deployment_id.to_owned(),
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
        log_sources(page),
        [(SERVICE_NAME, DeploymentLogSourceState::Available as i32)]
    );
}

fn log_sources(page: &ReadDeploymentLogsResponse) -> Vec<(&str, i32)> {
    page.sources
        .iter()
        .map(|source| (source.source_name.as_str(), source.state))
        .collect()
}

/// Returns the reconcile generation the simulated runtime printed in a page's first line.
fn simulated_log_generation(page: &ReadDeploymentLogsResponse) -> u64 {
    let first = page.entries.first().expect("a simulated log line");
    String::from_utf8_lossy(&first.content)
        .split_once("generation=")
        .and_then(|(_, rest)| rest.split_once(' '))
        .and_then(|(generation, _)| generation.parse().ok())
        .expect("Simulated line names its generation")
}

/// The simulated runtime's deterministic log line `line` of the deployment's service.
fn simulated_log_line(deployment_id: &str, generation: u64, line: u64) -> Vec<u8> {
    format!(
        "simulated workload log line {line:03} owner={deployment_id} generation={generation} \
         unit={SERVICE_NAME}"
    )
    .into_bytes()
}

/// The simulated runtime's timestamp of log line `line`: two seconds apart from a fixed epoch.
fn simulated_log_timestamp(line: u64) -> dotcore_proto::google::protobuf::Timestamp {
    dotcore_proto::google::protobuf::Timestamp {
        seconds: 1_700_000_000 + 2 * line as i64,
        nanos: 0,
    }
}

/// Returns the outcomes of one runtime event kind for the deployment, oldest first.
fn runtime_events<'a>(
    records: &'a [serde_json::Value],
    deployment_id: &str,
    event_kind: &str,
) -> Vec<&'a str> {
    records
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == event_kind
                && record["fields"]["deployment_id"] == deployment_id
        })
        .filter_map(|record| record["fields"]["outcome"].as_str())
        .collect()
}

/// Returns the route names the worker projected into the deployment's launches, oldest first.
fn launched_route_names<'a>(records: &'a [serde_json::Value], deployment_id: &str) -> Vec<&'a str> {
    records
        .iter()
        .filter(|record| {
            record["fields"]["event_kind"] == "application_launch_projected"
                && record["fields"]["deployment_id"] == deployment_id
        })
        .filter_map(|record| record["fields"]["route_name"].as_str())
        .collect()
}
