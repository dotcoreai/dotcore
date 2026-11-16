//! Proves, over the composed site manager, that the manager's own SiteNode realizes its model
//! and application deployments through the same node composition as any node: leased runtime
//! authority, in-process serving and deployment logs without a private listener, teardown through
//! cleanup evidence and a receipt, and restarts that find running workloads instead of
//! relaunching them.
//!
//! Each test drives a whole manager composition over a simulated site in a temporary directory,
//! restarting it in place when the scenario needs a manager restart.

use std::{net::SocketAddr, path::PathBuf, time::Duration};

use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationLifecycleState, CreateApplicationDeploymentRequest,
        ListApplicationDeploymentsRequest, ReadDeploymentLogsRequest as ReadApplicationLogsRequest,
        RemoveApplicationDeploymentRequest,
        application_deployment_service_server::ApplicationDeploymentService as ApplicationServiceContract,
    },
    deployment::v1::{ArtifactPreparationPolicy, ExactPlacement},
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection, InferenceRoute,
        ReplaceRouteRequest, route_service_server::RouteService as RouteServiceContract,
    },
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DeploymentLogHistoryState,
        DeploymentLogSourceState, DirectRouteExposure, FollowDeploymentLogsRequest,
        ListModelDeploymentsRequest, ModelDeploymentDeclaration, ModelDeploymentExposure,
        ModelDeploymentStatus, ReadDeploymentLogsRequest, RemoveModelDeploymentRequest,
        follow_deployment_logs_response, model_deployment_exposure,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        model_deployment_snapshot,
    },
};
use dotcore_runtime_backends::simulated::{SimulatedRuntimeGroupCall, SimulatedRuntimeTestSupport};
use dotcore_site::{
    node::control::ManagerNodeControlCoordinator,
    repository::SiteRepository,
    store::{FileCurrentState, FileNodeState},
};
use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tonic::Request;

use super::{
    BackendServices, BackendTestSupport, ManagerBackendServices, SelectedModelService,
    backend::ObserverTasks, compose_capabilities, default_collector, node::NodeLifecycle,
};
use crate::{
    application::repository::CurrentApplicationRepository as _,
    model::controller::serving_targets::NodeTransport,
    router::{
        adapters::{AdapterAttempt, AdapterResponse},
        selection::SelectedInferenceTarget,
    },
};

#[tokio::test]
async fn manager_node_runtime_actions_run_under_leased_authority() {
    let site = LocalNodeSite::new();
    let harness = LocalNodeHarness::start(&site, BackendTestSupport::test()).await;
    let deployment_id = harness.create(None).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;
    let declaration = site
        .declaration(&deployment_id)
        .expect("declared deployment");

    let permit = harness
        .node
        .model_runtimes
        .authorize_deployment_action(&declaration)
        .expect("assigned deployment action");

    assert!(
        permit.view_revision().is_some(),
        "the permit names the accepted view revision of a leased authority"
    );
    assert_eq!(
        permit.manager_session(),
        Some(harness.coordinator.manager_session())
    );
    let unassigned = ModelDeploymentDeclaration {
        model_deployment_id: "unassigned".to_owned(),
        ..declaration
    };
    assert!(
        harness
            .node
            .model_runtimes
            .authorize_deployment_action(&unassigned)
            .is_err(),
        "a deployment the accepted view does not assign gets no runtime action"
    );
}

#[tokio::test]
async fn manager_node_inference_and_logs_go_in_process_without_a_private_listener() {
    let site = LocalNodeSite::new();
    let harness = LocalNodeHarness::start(&site, BackendTestSupport::test()).await;
    let deployment_id = harness.create(Some("manager-route")).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;

    let attempt = harness.complete_chat("manager-route").await;
    assert!(matches!(
        attempt,
        AdapterAttempt::Response(AdapterResponse::Complete(_))
    ));

    let history = ModelServiceContract::read_deployment_logs(
        &harness.model_service,
        Request::new(ReadDeploymentLogsRequest {
            model_deployment_id: deployment_id.clone(),
            page_size: 5,
            ..Default::default()
        }),
    )
    .await
    .expect("read manager-node logs")
    .into_inner();
    assert_eq!(history.sources.len(), 1);
    assert_eq!(
        history.sources[0].state,
        DeploymentLogSourceState::Available as i32
    );
    assert_eq!(history.entries.len(), 5);
    assert!(history.entries.iter().all(|entry| {
        String::from_utf8_lossy(&entry.content).contains(&format!("owner={deployment_id}"))
    }));
    assert_eq!(
        history.history_state,
        DeploymentLogHistoryState::OlderOutputMayRemain as i32
    );

    let mut follow = ModelServiceContract::follow_deployment_logs(
        &harness.model_service,
        Request::new(FollowDeploymentLogsRequest {
            model_deployment_id: deployment_id,
            ..Default::default()
        }),
    )
    .await
    .expect("follow manager-node logs")
    .into_inner();
    let opened = tokio::time::timeout(Duration::from_secs(5), follow.next())
        .await
        .expect("follow event")
        .expect("open follow")
        .expect("follow response")
        .event
        .expect("event");
    assert!(matches!(
        opened,
        follow_deployment_logs_response::Event::SourceEvent(event)
            if event.state == DeploymentLogSourceState::Available as i32
    ));
}

#[tokio::test]
async fn manager_node_teardown_completes_through_cleanup_evidence_and_a_receipt_across_a_restart() {
    let site = LocalNodeSite::new();
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(0)
        .with_lifecycle_delays(Duration::ZERO, Duration::from_millis(1_500))
        .with_retained_workloads();
    let test_support = BackendTestSupport {
        simulated_runtime: Some(runtime),
        ..BackendTestSupport::test()
    };
    let harness = LocalNodeHarness::start(&site, test_support.clone()).await;
    let deployment_id = harness.create(None).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;

    ModelServiceContract::remove_model_deployment(
        &harness.model_service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id.clone(),
        }),
    )
    .await
    .expect("unload manager-node deployment");
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Unloading)
        .await;
    drop(harness);
    assert!(
        site.declaration(&deployment_id).is_some(),
        "the teardown is still waiting for the node's cleanup evidence at restart"
    );

    site.wait_for_release().await;
    let restarted = LocalNodeHarness::start(&site, test_support).await;
    restarted.wait_for_removal(&deployment_id).await;

    assert_eq!(site.declaration(&deployment_id), None);
    // The node retires its evidence once the receipt acknowledged it and the view dropped the
    // teardown.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !restarted
            .node
            .node_state
            .node_model_cleanup_evidence()
            .expect("retained cleanup evidence")
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("acknowledged and retired cleanup evidence");
}

#[tokio::test]
async fn a_manager_restart_finds_running_manager_node_workloads_without_relaunching_and_resumes_serving()
 {
    let site = LocalNodeSite::new();
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(0)
        .with_lifecycle_delays(Duration::ZERO, Duration::ZERO)
        .with_retained_workloads();
    let test_support = BackendTestSupport {
        simulated_runtime: Some(runtime.clone()),
        ..BackendTestSupport::test()
    };
    let harness = LocalNodeHarness::start(&site, test_support.clone()).await;
    let deployment_id = harness.create(Some("restart-route")).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;
    let launched = runtime.call_counts();
    assert_eq!(launched.creates, 1);
    drop(harness);

    site.wait_for_release().await;
    let restarted = LocalNodeHarness::start(&site, test_support).await;
    restarted
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;
    let attempt = restarted.complete_chat("restart-route").await;

    assert!(matches!(
        attempt,
        AdapterAttempt::Response(AdapterResponse::Complete(_))
    ));
    let after = runtime.call_counts();
    assert_eq!(
        (after.creates, after.starts, after.stops),
        (launched.creates, launched.starts, 0),
        "the restarted node adopts the running workload"
    );
}

#[tokio::test]
async fn the_manager_node_pins_a_synthetic_local_development_image_and_the_site_records_it() {
    // A simulated target pins without the node's image store, which holds no image here.
    let site = LocalNodeSite::new().with_tag_only_engine();
    let harness = LocalNodeHarness::start(&site, BackendTestSupport::test()).await;

    let deployment_id = harness.create(None).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;

    let recorded = site.recorded_launch_identities(&deployment_id).await;
    let [pinned] = recorded.as_slice() else {
        panic!("one recorded pin, got {recorded:?}");
    };
    assert!(
        dotcore_domain::oci::valid_local_image_id(pinned),
        "{pinned}"
    );
    assert_eq!(harness.retained_launch_identities(), recorded);
}

#[tokio::test]
async fn the_manager_node_pins_a_development_source_engine_once_and_keeps_it_across_a_restart() {
    let site = LocalNodeSite::new().with_development_source_engine();
    let harness = LocalNodeHarness::start(&site, BackendTestSupport::test()).await;
    let deployment_id = harness.create(None).await;
    harness
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;

    let accepted = site
        .declaration(&deployment_id)
        .and_then(|declaration| declaration.snapshot)
        .and_then(|snapshot| match snapshot.source {
            Some(model_deployment_snapshot::Source::Local(local)) => local.engine_artifact,
            _ => None,
        })
        .expect("accepted engine artifact");
    assert_eq!(
        (
            accepted.preparation_policy,
            accepted.launch_identity.as_str()
        ),
        (ArtifactPreparationPolicy::DevelopmentSource as i32, ""),
        "the site accepts the development-source engine unpinned"
    );
    let recorded = site.recorded_launch_identities(&deployment_id).await;
    let [pinned] = recorded.as_slice() else {
        panic!("one recorded pin, got {recorded:?}");
    };
    // A simulated target pins a synthetic store path, never evaluating the development source.
    assert!(dotcore_domain::nix::valid_store_path(pinned), "{pinned}");
    assert!(
        pinned.ends_with(&format!(
            "-simulated-{}",
            DEVELOPMENT_SOURCE_PACKAGE.replace('.', "-")
        )),
        "{pinned}"
    );
    assert_eq!(harness.retained_launch_identities(), recorded);
    drop(harness);

    // A restarted manager keeps the store path its node pinned first.
    site.wait_for_release().await;
    let restarted = LocalNodeHarness::start(&site, BackendTestSupport::test()).await;
    restarted
        .wait_for_status(&deployment_id, ModelDeploymentStatus::Ready)
        .await;

    assert_eq!(restarted.retained_launch_identities(), recorded);
    assert_eq!(
        site.recorded_launch_identities(&deployment_id).await,
        recorded
    );
}

#[tokio::test]
async fn exactly_one_node_realizer_acts_on_a_manager_node_application_and_reports_its_status() {
    let site = LocalNodeSite::new();
    let health = HealthServer::start().await;
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let harness = LocalNodeHarness::start(
        &site,
        BackendTestSupport {
            simulated_runtime: Some(runtime.clone()),
            ..BackendTestSupport::test()
        },
    )
    .await;

    let deployment_id = harness.create_application(None).await;
    let ready = harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
        })
        .await;

    let state = ready.state.expect("Ready state");
    assert!(!state.observation_stale);
    assert_eq!(
        state.endpoint.map(|endpoint| endpoint.url),
        Some(health.url()),
        "the owner projects the endpoint the node published on its node host"
    );
    assert_eq!(
        runtime
            .group_calls()
            .into_iter()
            .filter(|call| *call == SimulatedRuntimeGroupCall::create(&deployment_id))
            .count(),
        1,
        "exactly one realizer launched the deployment's group"
    );
    // The node keeps its realization state in its own store; the owner record is the accepted
    // status, written only by the owner.
    let node_records = harness
        .node
        .node_state
        .node_application_realizations()
        .expect("node realization records");
    assert_eq!(
        node_records
            .iter()
            .map(|record| record.application_deployment_id.as_str())
            .collect::<Vec<_>>(),
        [deployment_id.as_str()]
    );
    let node_current = harness
        .node
        .node_state
        .snapshot()
        .expect("node application records")
        .deployments;
    assert_eq!(node_current.len(), 1);
    assert_eq!(node_current[0].reconciled_lifecycle_generation, Some(1));
}

#[tokio::test]
async fn a_manager_node_application_relaunches_when_its_linked_route_is_renamed() {
    let site = LocalNodeSite::new();
    let health = HealthServer::start().await;
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let harness = LocalNodeHarness::start(
        &site,
        BackendTestSupport {
            simulated_runtime: Some(runtime.clone()),
            ..BackendTestSupport::test()
        },
    )
    .await;
    let model_deployment_id = harness.create(None).await;
    let route = harness.create_route(&model_deployment_id, "chat").await;

    let deployment_id = harness.create_application(Some(&route)).await;
    harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
        })
        .await;
    RouteServiceContract::replace_route(
        &harness.manager.route_service,
        Request::new(ReplaceRouteRequest {
            inference_route_id: route.inference_route_id.clone(),
            expected_generation: route.generation,
            name: "chat-renamed".to_owned(),
            enabled: true,
            fallback_groups: route.fallback_groups.clone(),
        }),
    )
    .await
    .expect("renamed route");
    harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment
                .selected_inference_route
                .as_ref()
                .is_some_and(|route| route.route_name == "chat-renamed")
                && deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
                && launched_route_names(&runtime, &deployment_id).last()
                    == Some(&"chat-renamed".to_owned())
        })
        .await;

    assert_eq!(
        launched_route_names(&runtime, &deployment_id),
        ["chat", "chat-renamed"],
        "the node launches once per projected route name"
    );
    assert!(
        runtime
            .group_calls()
            .contains(&SimulatedRuntimeGroupCall::remove(&deployment_id)),
        "the rename replaces the group launched with the old name"
    );
}

#[tokio::test]
async fn manager_node_application_teardown_completes_through_cleanup_evidence_and_a_receipt() {
    let site = LocalNodeSite::new();
    let health = HealthServer::start().await;
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let harness = LocalNodeHarness::start(
        &site,
        BackendTestSupport {
            simulated_runtime: Some(runtime.clone()),
            ..BackendTestSupport::test()
        },
    )
    .await;
    let deployment_id = harness.create_application(None).await;
    harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
        })
        .await;

    ApplicationServiceContract::remove_application_deployment(
        &harness.application_service,
        Request::new(RemoveApplicationDeploymentRequest {
            application_deployment_id: deployment_id.clone(),
        }),
    )
    .await
    .expect("teardown intent");
    harness.wait_for_application_removal(&deployment_id).await;

    assert!(!site.declares_application(&deployment_id));
    let calls = runtime.group_calls();
    assert!(calls.contains(&SimulatedRuntimeGroupCall::ensure_absent(&deployment_id)));
    assert!(calls.contains(&SimulatedRuntimeGroupCall::remove_storage(&deployment_id)));
    assert!(
        !runtime.storage_exists(&dotcore_runtime::WorkloadStorageKey {
            kind: dotcore_runtime::WorkloadStorageKind::Data,
            namespace: crate::storage::DEPLOYMENT_NAMESPACE.to_owned(),
            key: deployment_id.clone(),
        })
    );
    // The owner finalized the teardown from the node's evidence, and the node retires that
    // evidence once the receipt acknowledged it and the view dropped the teardown.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !harness
            .node
            .node_state
            .node_application_cleanup_evidence()
            .expect("retained application cleanup evidence")
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("acknowledged and retired application cleanup evidence");
    assert!(
        harness
            .node
            .node_state
            .node_application_realizations()
            .expect("node realization records")
            .is_empty()
    );
}

#[tokio::test]
async fn a_manager_restart_finds_a_running_manager_node_application_without_relaunching_it() {
    let site = LocalNodeSite::new();
    let health = HealthServer::start().await;
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port())
        .with_retained_workloads();
    let test_support = BackendTestSupport {
        simulated_runtime: Some(runtime.clone()),
        ..BackendTestSupport::test()
    };
    let harness = LocalNodeHarness::start(&site, test_support.clone()).await;
    let deployment_id = harness.create_application(None).await;
    harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
        })
        .await;
    let launched = runtime.group_calls();
    drop(harness);

    site.wait_for_release().await;
    let restarted = LocalNodeHarness::start(&site, test_support).await;
    let ready = restarted
        .wait_for_application(&deployment_id, |deployment| {
            deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                    && !state.observation_stale
            })
        })
        .await;

    assert_eq!(
        ready
            .state
            .and_then(|state| state.endpoint)
            .map(|endpoint| endpoint.url),
        Some(health.url())
    );
    let mutations = |calls: &[SimulatedRuntimeGroupCall]| {
        calls
            .iter()
            .filter(|call| call.operation != "inspect")
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        mutations(&runtime.group_calls()),
        mutations(&launched),
        "the restarted node adopts the running group without a runtime mutation"
    );
}

#[tokio::test]
async fn manager_node_application_logs_read_in_process_without_a_private_listener() {
    let site = LocalNodeSite::new();
    let health = HealthServer::start().await;
    let harness = LocalNodeHarness::start(
        &site,
        BackendTestSupport {
            simulated_runtime: Some(SimulatedRuntimeTestSupport::with_application_endpoint(
                health.port(),
            )),
            ..BackendTestSupport::test()
        },
    )
    .await;
    let deployment_id = harness.create_application(None).await;
    harness
        .wait_for_application(&deployment_id, |deployment| {
            deployment_lifecycle(deployment) == Some(ApplicationLifecycleState::Ready)
        })
        .await;

    let history = ApplicationServiceContract::read_deployment_logs(
        &harness.application_service,
        Request::new(ReadApplicationLogsRequest {
            application_deployment_id: deployment_id.clone(),
            page_size: 5,
            ..Default::default()
        }),
    )
    .await
    .expect("read manager-node application logs")
    .into_inner();

    assert_eq!(
        history
            .sources
            .iter()
            .map(|source| (source.source_name.as_str(), source.state))
            .collect::<Vec<_>>(),
        [("web", DeploymentLogSourceState::Available as i32)]
    );
    assert!(!history.entries.is_empty());
    assert!(history.entries.iter().all(|entry| {
        String::from_utf8_lossy(&entry.content).contains(&format!("owner={deployment_id}"))
    }));
}

/// A manager composition over one simulated site, with every observer started as a serving
/// backend starts them.
struct LocalNodeHarness {
    model_service: SelectedModelService,
    application_service: super::SelectedApplicationService,
    manager: ManagerBackendServices,
    coordinator: ManagerNodeControlCoordinator,
    node: NodeLifecycle,
    _observers: ObserverTasks,
}

impl LocalNodeHarness {
    async fn start(site: &LocalNodeSite, test_support: BackendTestSupport) -> Self {
        let BackendServices {
            model_service,
            application_service,
            manager,
            observers,
            node_lifecycle,
            node,
        } = compose_capabilities(&site.paths, None, 0, test_support, default_collector())
            .await
            .expect("manager composition");
        Self {
            model_service,
            application_service,
            manager,
            coordinator: node_lifecycle,
            node,
            _observers: observers.start(),
        }
    }

    /// Creates the fixture application on the manager node's application target, linked to
    /// `route` when given, and returns its ID.
    async fn create_application(&self, route: Option<&InferenceRoute>) -> String {
        ApplicationServiceContract::create_application_deployment(
            &self.application_service,
            Request::new(CreateApplicationDeploymentRequest {
                application_id: LOCAL_APPLICATION_ID.to_owned(),
                placement: Some(ExactPlacement {
                    site_node_id: "fixture-node".to_owned(),
                    execution_target_id: "fixture-application-target".to_owned(),
                }),
                inference_route_id: route.map(|route| route.inference_route_id.clone()),
                expected_inference_route_generation: route.map_or(0, |route| route.generation),
                application_deployment_name: None,
                parameter_values: Vec::new(),
                storage_choices: Vec::new(),
            }),
        )
        .await
        .expect("create manager-node application")
        .into_inner()
        .deployment
        .expect("created application")
        .application_deployment_id
    }

    /// Creates an enabled route named `name` whose only member is `model_deployment_id`.
    async fn create_route(&self, model_deployment_id: &str, name: &str) -> InferenceRoute {
        RouteServiceContract::create_route(
            &self.manager.route_service,
            Request::new(CreateRouteRequest {
                name: name.to_owned(),
                enabled: true,
                fallback_groups: vec![FallbackGroup {
                    selection: FallbackGroupSelection::Single as i32,
                    model_deployment_ids: vec![model_deployment_id.to_owned()],
                }],
            }),
        )
        .await
        .expect("created route")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("route declaration")
    }

    /// Waits until the owner projects the application so that `matches` holds.
    async fn wait_for_application(
        &self,
        deployment_id: &str,
        matches: impl Fn(&ApplicationDeployment) -> bool,
    ) -> ApplicationDeployment {
        let mut last = None;
        let reached = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let deployment = self.application(deployment_id).await;
                if deployment.as_ref().is_some_and(&matches) {
                    return deployment.expect("matched application");
                }
                last = deployment;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        reached
            .unwrap_or_else(|_| panic!("application {deployment_id} never matched; last {last:?}"))
    }

    /// Waits until the owner no longer lists the application.
    async fn wait_for_application_removal(&self, deployment_id: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.application(deployment_id).await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("finalized application teardown");
    }

    async fn application(&self, deployment_id: &str) -> Option<ApplicationDeployment> {
        ApplicationServiceContract::list_application_deployments(
            &self.application_service,
            Request::new(ListApplicationDeploymentsRequest {}),
        )
        .await
        .expect("list applications")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
    }

    /// Creates the fixture deployment on the manager node's model target, exposed directly as
    /// `route` or hidden, and returns its ID.
    async fn create(&self, route: Option<&str>) -> String {
        let kind = match route {
            Some(route_name) => model_deployment_exposure::Kind::Direct(DirectRouteExposure {
                route_name: route_name.to_owned(),
            }),
            None => model_deployment_exposure::Kind::Hidden(Default::default()),
        };
        ModelServiceContract::create_model_deployment(
            &self.model_service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                        CreateLocalModelDeploymentRequest {
                            model_id: "fixture-model".to_owned(),
                            profile_id: "fixture-profile".to_owned(),
                            placement: Some(ExactPlacement {
                                site_node_id: "fixture-node".to_owned(),
                                execution_target_id: "fixture-target".to_owned(),
                            }),
                            configuration: Some(crate::test_support::local_model_configuration()),
                        },
                    ),
                ),
                exposure: Some(ModelDeploymentExposure { kind: Some(kind) }),
                model_deployment_name: None,
            }),
        )
        .await
        .expect("create manager-node deployment")
        .into_inner()
        .deployment
        .expect("created deployment")
        .model_deployment_id
    }

    /// Waits until the owner projects `expected` for the deployment, as its node reported it.
    async fn wait_for_status(&self, deployment_id: &str, expected: ModelDeploymentStatus) {
        let mut last = None;
        let reached = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                last = self.status(deployment_id).await;
                if last
                    .as_ref()
                    .is_some_and(|(status, _)| *status == expected as i32)
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            reached.is_ok(),
            "deployment {deployment_id} never reached {expected:?}; last status {last:?}"
        );
    }

    /// Waits until the owner no longer lists the deployment.
    async fn wait_for_removal(&self, deployment_id: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.status(deployment_id).await.is_some() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("finalized teardown");
    }

    /// Returns every launch identity the manager node retains in its node store.
    fn retained_launch_identities(&self) -> Vec<String> {
        self.node
            .node_state
            .node_model_resolved_launch_identities()
            .expect("retained node pins")
            .into_iter()
            .flat_map(|record| record.resolved_launch_identities)
            .map(|pin| pin.launch_identity)
            .collect()
    }

    async fn status(&self, deployment_id: &str) -> Option<(i32, String)> {
        ModelServiceContract::list_model_deployments(
            &self.model_service,
            Request::new(ListModelDeploymentsRequest {}),
        )
        .await
        .expect("list deployments")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .map(|deployment| {
            let state = deployment.state.unwrap_or_default();
            (state.status, state.status_message)
        })
    }

    /// Sends one non-streaming chat through the manager's router to `route`, which must select
    /// the manager node's target for in-process dispatch.
    async fn complete_chat(&self, route: &str) -> AdapterAttempt {
        let body =
            format!(r#"{{"model":"{route}","messages":[{{"role":"user","content":"Hello"}}]}}"#);
        let crate::router::openai::OpenAiRequest::Chat(request) =
            crate::router::openai::decode("POST", "/v1/chat/completions", body.as_bytes())
                .expect("chat request")
        else {
            panic!("a chat completion request decodes as Chat");
        };
        let request = crate::inference::InferenceRequest::Chat(request);
        let router = &self.manager.inference_router;
        let lease = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(lease) = router.admit(&request) {
                    return lease;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("an eligible manager-node target");
        let (_client, client_cancellation) = tokio::sync::watch::channel(None);
        let (target, cancellation) = lease.into_request(client_cancellation);
        assert!(matches!(
            &target,
            SelectedInferenceTarget::Peer(peer)
                if peer.dispatch().transport == NodeTransport::InProcess
        ));
        crate::router::adapters::invoke(target, request, cancellation, router.peer_client()).await
    }
}

/// A simulated single-node site in a temporary directory, kept across manager restarts.
struct LocalNodeSite {
    root: PathBuf,
    paths: dotcore_domain::SitePaths,
}

impl LocalNodeSite {
    fn new() -> Self {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../backend/fixtures/simulated-site");
        let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
            "dotcore-local-node-{}",
            dotcore_domain::test_support::unique_suffix()
        ));
        for directory in [
            "catalog/models",
            "catalog/engines",
            "catalog/applications",
            "catalog/hardware-classes",
        ] {
            std::fs::create_dir_all(root.join(directory)).expect("fixture directory");
        }
        std::fs::write(
            root.join(format!("catalog/applications/{LOCAL_APPLICATION_ID}.yaml")),
            LOCAL_APPLICATION,
        )
        .expect("fixture application");
        for file in [
            "site.yaml",
            "node.yaml",
            "catalog/models/fixture-model.yaml",
            "catalog/engines/fixture.yaml",
            "catalog/hardware-classes/fixture-nvidia.yaml",
        ] {
            std::fs::copy(source.join(file), root.join(file)).expect("fixture file");
        }
        let master = root.join("master-catalog");
        dotcore_catalog::test_support::create_empty_catalog_source(&master);
        dotcore_catalog::test_support::write_hardware_classes(&master);
        let paths = dotcore_domain::SitePaths::from_values(
            Some(root.to_str().expect("fixture path")),
            Some(master.to_str().expect("master catalog path")),
            Some(root.join("cache").to_str().expect("cache path")),
        )
        .expect("Site paths");
        Self { root, paths }
    }

    /// Declares the fixture engine image by tag only, as a local-development image that its
    /// hosting node pins.
    fn with_tag_only_engine(self) -> Self {
        let engine = self.root.join("catalog/engines/fixture.yaml");
        let digest = r#"digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }"#;
        let source = std::fs::read_to_string(&engine).expect("fixture engine");
        assert!(source.contains(digest), "fixture engine is digest-pinned");
        std::fs::write(&engine, source.replace(digest, r#"tag: "dev""#)).expect("tag-only engine");
        self
    }

    /// Moves the node to a declared Mac and its model target to darwin/arm64 on the Mac's GPU, and
    /// declares the fixture engine as package
    /// [`DEVELOPMENT_SOURCE_PACKAGE`] of the development source (`self`), which its hosting node
    /// pins, with the model requiring an Apple accelerator.
    fn with_development_source_engine(self) -> Self {
        let rewrite = |file: &str, from: &str, to: &str| {
            let path = self.root.join(file);
            let source = std::fs::read_to_string(&path).expect("fixture file");
            assert!(source.contains(from), "{file} fixture");
            std::fs::write(&path, source.replacen(from, to, 1)).expect("rewritten fixture");
        };
        let node_path = self.root.join("node.yaml");
        let mut node: dotcore_proto::dotcore::node::v1::NodeConfiguration =
            dotcore_proto::yaml::from_file(&node_path).expect("node.yaml fixture");
        node.declared_machine = Some(dotcore_proto::dotcore::node::v1::DeclaredMachine {
            choices: vec!["mac-64g".to_owned()],
            description: Some(
                dotcore_node_hardware::MachineDescription::named("mac-64g")
                    .expect("named machine")
                    .to_stored(),
            ),
        });
        std::fs::write(
            &node_path,
            dotcore_proto::yaml::to_string(&node).expect("node.yaml"),
        )
        .expect("rewritten node.yaml");
        rewrite(
            "site.yaml",
            "- executionTargetId: fixture-target\n  siteNodeId: fixture-node\n  displayName: Simulated models\n  platform: { operatingSystem: linux, architecture: amd64 }\n  acceleratorDeviceIds: [GPU-00000000-0000-0000-0000-000000005090]",
            "- executionTargetId: fixture-target\n  siteNodeId: fixture-node\n  displayName: Simulated models\n  platform: { operatingSystem: darwin, architecture: arm64 }\n  acceleratorDeviceIds: [apple-gpu-0]",
        );
        rewrite(
            "catalog/engines/fixture.yaml",
            r#"    oci:
      repository:
        registry: { host: "registry.dotcore.test" }
        name: "fixture"
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580"#,
            r#"    nix: { flake: "self", package: "engines.llama-cpp.native", executable: "llama-server" }
    selectors:
      - platform: { operatingSystem: "darwin", architecture: "arm64" }
        accelerators: ["apple"]"#,
        );
        rewrite(
            "catalog/models/fixture-model.yaml",
            r#"hardwareClassId: "fixture-nvidia""#,
            r#"hardwareClassId: "apple""#,
        );
        self
    }

    /// Waits until the site records node-resolved launch identities for the deployment, then
    /// returns them.
    async fn recorded_launch_identities(&self, deployment_id: &str) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let declaration = self
                    .declaration(deployment_id)
                    .expect("declared deployment");
                if !declaration.node_resolved_launch_identities.is_empty() {
                    return declaration
                        .node_resolved_launch_identities
                        .into_iter()
                        .map(|pin| pin.launch_identity)
                        .collect();
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("recorded node-resolved pin")
    }

    /// Returns the deployment's declaration as the site retains it.
    fn declaration(&self, deployment_id: &str) -> Option<ModelDeploymentDeclaration> {
        SiteRepository::load(
            &self.root.join("site.yaml"),
            crate::test_support::site_declaration_rules(),
        )
        .expect("retained site")
        .snapshot()
        .expect("Site snapshot")
        .model_deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
    }

    /// Whether the site still declares the application deployment.
    fn declares_application(&self, deployment_id: &str) -> bool {
        SiteRepository::load(
            &self.root.join("site.yaml"),
            crate::test_support::site_declaration_rules(),
        )
        .expect("retained site")
        .snapshot()
        .expect("Site snapshot")
        .application_deployments
        .iter()
        .any(|deployment| deployment.application_deployment_id == deployment_id)
    }

    /// Waits until a dropped composition's asynchronously stopped tasks released both stores.
    async fn wait_for_release(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let owner = FileCurrentState::open(self.root.join("site.yaml"), "fixture-site");
                let node =
                    FileNodeState::open(self.root.join("node"), "fixture-site", "fixture-node");
                if owner.is_ok() && node.is_ok() {
                    return;
                }
                drop((owner, node));
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("released site stores");
    }
}

impl Drop for LocalNodeSite {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Readiness endpoint on the fixture SiteNode's host, answering every request with 200.
struct HealthServer {
    address: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl HealthServer {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("readiness listener");
        let address = listener.local_addr().expect("readiness address");
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut scratch = [0_u8; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut scratch).await {
                            Ok(read @ 1..) => request.extend_from_slice(&scratch[..read]),
                            _ => return,
                        }
                    }
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                        )
                        .await;
                });
            }
        });
        Self { address, task }
    }

    fn port(&self) -> u16 {
        self.address.port()
    }

    /// The direct endpoint URL the node publishes and the owner projects.
    fn url(&self) -> String {
        format!("http://{}", self.address)
    }
}

impl Drop for HealthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn deployment_lifecycle(deployment: &ApplicationDeployment) -> Option<ApplicationLifecycleState> {
    deployment
        .state
        .as_ref()
        .and_then(|state| ApplicationLifecycleState::try_from(state.lifecycle_state).ok())
}

/// The `ROUTE_NAME` each launch of the deployment's group carried, in launch order.
fn launched_route_names(runtime: &SimulatedRuntimeTestSupport, deployment_id: &str) -> Vec<String> {
    runtime
        .group_launches()
        .iter()
        .filter(|launch| launch.correlation.owner_id == deployment_id)
        .filter_map(|launch| {
            launch
                .units
                .values()
                .find_map(|unit| unit.environment.get("ROUTE_NAME").cloned())
        })
        .collect()
}

/// Development-source engine package the Nix fixture engine declares.
const DEVELOPMENT_SOURCE_PACKAGE: &str = "engines.llama-cpp.native";

const LOCAL_APPLICATION_ID: &str = "local-fixture";

/// Synthetic application: one digest-pinned service with a readiness check, retained storage,
/// and an optional route-name link with an empty literal fallback.
const LOCAL_APPLICATION: &str = r#"applicationId: "local-fixture"
displayName: "Local fixture"
version: "1"
graph:
  services:
    - serviceName: "web"
      artifact:
        oci:
          repository:
            registry: { host: "registry.dotcore.test" }
            name: "local-fixture"
          digest:
            algorithm: "sha256"
            encoded: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        supportedPlatforms: [{ operatingSystem: "linux", architecture: "amd64" }]
      ports: [{ portName: "http", portNumber: 8080 }]
      configurationBindings:
        - bindingName: "route-name"
          environmentVariable: "ROUTE_NAME"
          literalValue: ""
          input:
            inferenceRoute:
              projection: "APPLICATION_INFERENCE_ROUTE_PROJECTION_ROUTE_NAME"
      storageBindings: [{ bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_DATA", placements: ["STORAGE_PLACEMENT_DEPLOYMENT"] }]
      readiness: { httpGet: { portName: "http", path: "/health" } }
  primaryEndpoint:
    serviceName: "web"
    portName: "http"
    protocol: "APPLICATION_ENDPOINT_PROTOCOL_HTTP"
"#;
