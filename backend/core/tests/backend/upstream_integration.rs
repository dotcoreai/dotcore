//! Whole-backend integration coverage for deterministic OpenAI-compatible upstream fixtures.

use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use dotcore::{
    composition::BackendTestSupport,
    testing::{ManualClock, Timing},
};
use dotcore_domain::timing::ModelTiming;
use dotcore_proto::dotcore::{
    catalog::v1::{UpstreamModelBoundary, UpstreamModelDataEgressClass, UpstreamModelLocality},
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection, InferenceRoute,
        InferenceRouteView, ListRoutesRequest, ReplaceRouteRequest,
    },
    model::v1::{
        CreateModelDeploymentRequest, CreateUpstreamModelDeploymentRequest, DirectRouteExposure,
        ModelDeployment, ModelDeploymentExposure, ModelDeploymentMetadata, ModelDeploymentStatus,
        OpenAiCompatibleEndpointConfiguration, RemoveModelDeploymentRequest,
        UpdateModelDeploymentMetadataRequest, UpdateModelDeploymentRequest,
        UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
        model_deployment_exposure,
    },
};
use dotcore_proto::google::protobuf::FieldMask;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Notify, oneshot},
};
use tracing::{Event, Subscriber, field, span};
use tracing_subscriber::{
    Layer,
    layer::{Context, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

use crate::support;

const CREDENTIAL: &str = "fixture-secret-do-not-log";
const PUBLIC_MODEL: &str = "scripted-model";
const PRIVATE_MODEL: &str = "upstream-private-model";
const READY_TIMEOUT: Duration = Duration::from_secs(5);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
const LEASE_CONTINUITY_TIMEOUT: Duration = Duration::from_secs(35);

/// Post-outage recovery bound. A withdrawn upstream returns to service only
/// when the controller's next scheduled probe runs, so the wait spans the 10 s
/// initial retry delay, a worst-case two-request probe pass at the 5 s probe
/// timeout, and the lifecycle publication that follows.
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

fn install_credential_guard() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        tracing_subscriber::registry()
            .with(CredentialGuard)
            .try_init()
            .expect("upstream integration credential guard must install");
    });
}

struct CredentialGuard;

impl<S> Layer<S> for CredentialGuard
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, _id: &span::Id, _ctx: Context<'_, S>) {
        attrs.record(&mut CredentialVisitor);
    }

    fn on_record(&self, _id: &span::Id, values: &span::Record<'_>, _ctx: Context<'_, S>) {
        values.record(&mut CredentialVisitor);
    }

    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        event.record(&mut CredentialVisitor);
    }
}

struct CredentialVisitor;

impl field::Visit for CredentialVisitor {
    fn record_debug(&mut self, _field: &field::Field, value: &dyn std::fmt::Debug) {
        assert_observability_redacted(&format!("{value:?}"));
    }

    fn record_str(&mut self, _field: &field::Field, value: &str) {
        assert_observability_redacted(value);
    }
}

fn assert_observability_redacted(value: &str) {
    assert!(
        !value.contains(CREDENTIAL),
        "credential leaked into a trace or log record"
    );
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Clone, Default)]
struct UpstreamState {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    outage: Arc<Mutex<bool>>,
    outage_after_get_count: Arc<Mutex<Option<usize>>>,
    block_inference: Arc<AtomicBool>,
    inference_started: Arc<AtomicBool>,
    inference_started_signal: Arc<Notify>,
    inference_cancelled: Arc<AtomicBool>,
    inference_cancelled_signal: Arc<Notify>,
}

struct ScriptedUpstream {
    address: std::net::SocketAddr,
    state: UpstreamState,
    shutdown: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl ScriptedUpstream {
    async fn start() -> Self {
        let listener = tokio::time::timeout(READY_TIMEOUT, TcpListener::bind("127.0.0.1:0"))
            .await
            .expect("scripted upstream listener deadline")
            .expect("scripted upstream listener");
        let address = listener.local_addr().expect("scripted upstream address");
        let state = UpstreamState::default();
        let worker_state = state.clone();
        let (shutdown, mut signal) = oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut signal => return,
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { return };
                        let state = worker_state.clone();
                        tokio::spawn(async move { serve_request(stream, state).await });
                    }
                }
            }
        });
        Self {
            address,
            state,
            shutdown: Some(shutdown),
            task,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}/v1", self.address)
    }

    fn requests(&self) -> Vec<CapturedRequest> {
        self.state
            .requests
            .lock()
            .expect("upstream requests")
            .clone()
    }

    fn set_outage(&self, outage: bool) {
        *self.state.outage.lock().expect("upstream outage") = outage;
        if !outage {
            *self
                .state
                .outage_after_get_count
                .lock()
                .expect("upstream delayed outage") = None;
        }
    }

    fn fail_after_the_next_get(&self) {
        let next_get = self
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .count()
            + 1;
        *self
            .state
            .outage_after_get_count
            .lock()
            .expect("upstream delayed outage") = Some(next_get);
    }

    fn inference_attempt_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count()
    }

    fn block_inference(&self) {
        self.state.block_inference.store(true, Ordering::SeqCst);
    }

    async fn wait_for_blocked_inference(&self) {
        wait_for_signal(
            &self.state.inference_started,
            &self.state.inference_started_signal,
            "blocked upstream inference",
        )
        .await;
    }

    async fn wait_for_inference_cancellation(&self) {
        wait_for_signal(
            &self.state.inference_cancelled,
            &self.state.inference_cancelled_signal,
            "upstream inference cancellation",
        )
        .await;
    }

    async fn stop(mut self) {
        self.shutdown
            .take()
            .expect("upstream shutdown")
            .send(())
            .ok();
        tokio::time::timeout(READY_TIMEOUT, self.task)
            .await
            .expect("scripted upstream shutdown deadline")
            .expect("scripted upstream task");
    }
}

#[tokio::test]
async fn simulated_and_upstream_routes_coexist_through_independent_lifecycles() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let site_path = fixture.join("site.yaml");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    let simulated = model
        .create_model_deployment(local_creation_request("fixture-model".to_owned()))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let simulated_ready =
        wait_for_lifecycle(&mut model, &simulated, ModelDeploymentStatus::Ready).await;

    let duplicate = model
        .create_model_deployment(upstream_creation_request(
            "fixture-model".to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect_err("API must reject a duplicate simulated route");
    assert_eq!(duplicate.code(), tonic::Code::AlreadyExists);
    wait_for_model_ids(&backend, &["fixture-model"]).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;

    let upstream_deployment = model
        .create_model_deployment(upstream_creation_request(
            PUBLIC_MODEL.to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load distinct upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(
        &mut model,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;
    wait_for_model_ids(&backend, &["fixture-model", PUBLIC_MODEL]).await;

    let simulated_response = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"fixture-model","messages":[{"role":"user","content":"local"}]}"#,
    )
    .await;
    assert_eq!(simulated_response.status, 200);
    assert_eq!(json(&simulated_response)["model"], "fixture-model");
    assert_eq!(
        json(&simulated_response)["choices"][0]["message"]["content"],
        "Simulated response."
    );
    let upstream_attempts = upstream.inference_attempt_count();
    let upstream_response = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"remote"}]}"#,
    )
    .await;
    assert_eq!(upstream_response.status, 200);
    assert_eq!(json(&upstream_response)["model"], PUBLIC_MODEL);
    assert_one_inference_attempt(&upstream, upstream_attempts);
    assert_simulated_unchanged(&mut model, &simulated_ready).await;

    let edited = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: upstream_deployment.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v2",
            )),
        })
        .await
        .expect("edit upstream model")
        .into_inner()
        .deployment
        .expect("edited upstream deployment");
    wait_for_lifecycle(&mut model, &edited, ModelDeploymentStatus::Ready).await;
    wait_for_model_ids(&backend, &["fixture-model", PUBLIC_MODEL]).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;

    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: upstream_deployment.model_deployment_id,
        })
        .await
        .expect("unload upstream model");
    wait_for_model_ids(&backend, &["fixture-model"]).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;
    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn upstream_only_site_without_execution_targets_serves_and_unloads_its_route() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let site_path = fixture.join("site.yaml");
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("fixture site");
    site.execution_targets.clear();
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("target-less site YAML"),
    )
    .expect("target-less site fixture");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    let deployment = model
        .create_model_deployment(upstream_creation_request(
            PUBLIC_MODEL.to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(&mut model, &deployment, ModelDeploymentStatus::Ready).await;
    wait_for_model_ids(&backend, &[PUBLIC_MODEL]).await;
    let attempts = upstream.inference_attempt_count();
    let response = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"remote"}]}"#,
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["model"], PUBLIC_MODEL);
    assert_one_inference_attempt(&upstream, attempts);

    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        })
        .await
        .expect("unload upstream model");
    wait_for_removed(&mut model).await;
    wait_for_model_ids(&backend, &[]).await;
    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn direct_site_mixed_custom_route_projects_like_a_management_created_route() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let configuration = support::configuration(&fixture);
    let backend = support::RunningBackend::start(configuration.clone()).await;
    let mut models = backend.model_client();
    let simulated = models
        .create_model_deployment(hidden_local_creation_request("fixture-model"))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let upstream_deployment = models
        .create_model_deployment(hidden_upstream_creation_request(
            PUBLIC_MODEL,
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(&mut models, &simulated, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(
        &mut models,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;
    backend.stop().await;
    wait_for_state_release(&fixture).await;

    let site_path = fixture.join("site.yaml");
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("persisted site");
    let direct_route = InferenceRoute {
        inference_route_id: "site-mixed-route".to_owned(),
        name: "site-mixed-route".to_owned(),
        enabled: true,
        fallback_groups: mixed_custom_groups(
            &simulated.model_deployment_id,
            &upstream_deployment.model_deployment_id,
        ),
        generation: 1,
    };
    site.inference_routes.push(direct_route.clone());
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("mixed route site YAML"),
    )
    .expect("direct mixed route site fixture");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");

    let backend = support::RunningBackend::start(configuration).await;
    let mut models = backend.model_client();
    wait_for_lifecycle(&mut models, &simulated, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(
        &mut models,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;
    let direct_routes = wait_for_eligible_route_members(&backend, &direct_route.name).await;
    assert_eq!(direct_routes.len(), 1);
    let direct = direct_routes.into_iter().next().expect("direct site route");
    assert_eq!(direct.route.as_ref(), Some(&direct_route));

    let managed = backend
        .route_client()
        .create_route(CreateRouteRequest {
            name: "managed-mixed-route".to_owned(),
            enabled: true,
            fallback_groups: direct_route.fallback_groups.clone(),
        })
        .await
        .expect("create mixed route")
        .into_inner()
        .route
        .expect("managed route");
    assert_eq!(direct.availability, managed.availability);

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn management_created_mixed_custom_route_persists_and_reloads_with_the_same_projection() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut models = backend.model_client();
    let simulated = models
        .create_model_deployment(hidden_local_creation_request("fixture-model"))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let upstream_deployment = models
        .create_model_deployment(hidden_upstream_creation_request(
            PUBLIC_MODEL,
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(&mut models, &simulated, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(
        &mut models,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let fallback_groups = mixed_custom_groups(
        &simulated.model_deployment_id,
        &upstream_deployment.model_deployment_id,
    );
    let created = backend
        .route_client()
        .create_route(CreateRouteRequest {
            name: "mixed-route".to_owned(),
            enabled: true,
            fallback_groups: fallback_groups.clone(),
        })
        .await
        .expect("create mixed route")
        .into_inner()
        .route
        .expect("created route")
        .route
        .expect("created declaration");
    assert_eq!(created.name, "mixed-route");
    assert!(created.enabled);
    assert_eq!(created.fallback_groups, fallback_groups);
    assert_eq!(created.generation, 1);

    let persisted: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&fixture.join("site.yaml")).expect("persisted site");
    assert_eq!(persisted.inference_routes, std::slice::from_ref(&created));

    backend.stop().await;
    wait_for_state_release(&fixture).await;
    let reloaded = support::RunningBackend::start(support::configuration(&fixture)).await;
    let routes = reloaded
        .route_client()
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list reloaded routes")
        .into_inner()
        .routes;
    assert_eq!(routes.len(), 1);
    assert_eq!(
        routes[0].route.as_ref().expect("reloaded declaration"),
        &created
    );

    reloaded.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn mixed_custom_route_selects_controller_targets_by_declared_round_robin_policy() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut models = backend.model_client();
    let simulated = models
        .create_model_deployment(hidden_local_creation_request("fixture-model"))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let upstream_deployment = models
        .create_model_deployment(hidden_upstream_creation_request(
            PUBLIC_MODEL,
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(&mut models, &simulated, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(
        &mut models,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let created = backend
        .route_client()
        .create_route(CreateRouteRequest {
            name: "mixed-route".to_owned(),
            enabled: true,
            fallback_groups: vec![FallbackGroup {
                selection: FallbackGroupSelection::RoundRobin as i32,
                model_deployment_ids: vec![
                    simulated.model_deployment_id.clone(),
                    upstream_deployment.model_deployment_id.clone(),
                ],
            }],
        })
        .await
        .expect("create mixed route")
        .into_inner()
        .route
        .expect("created route")
        .route
        .expect("created declaration");
    assert_eq!(created.name, "mixed-route");
    assert_eq!(
        created.fallback_groups[0].selection,
        FallbackGroupSelection::RoundRobin as i32
    );
    wait_for_eligible_route_members(&backend, &created.name).await;

    let upstream_attempts = upstream.inference_attempt_count();
    let simulated_response = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"mixed-route","messages":[{"role":"user","content":"local"}]}"#,
    )
    .await;
    assert_eq!(simulated_response.status, 200);
    assert_eq!(json(&simulated_response)["model"], "mixed-route");
    assert_eq!(
        json(&simulated_response)["choices"][0]["message"]["content"],
        "Simulated response."
    );
    assert_eq!(upstream.inference_attempt_count(), upstream_attempts);

    let upstream_response = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"mixed-route","messages":[{"role":"user","content":"remote"}]}"#,
    )
    .await;
    assert_public_response(&upstream_response, &upstream);
    assert_eq!(upstream_response.status, 200);
    assert_eq!(json(&upstream_response)["model"], "mixed-route");
    assert_eq!(
        json(&upstream_response)["choices"][0]["message"]["content"],
        "Scripted response."
    );
    assert_one_inference_attempt(&upstream, upstream_attempts);

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn management_replaces_custom_route_with_mixed_ready_members_once() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut models = backend.model_client();
    let simulated = models
        .create_model_deployment(hidden_local_creation_request("fixture-model"))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let upstream_deployment = models
        .create_model_deployment(hidden_upstream_creation_request(
            PUBLIC_MODEL,
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(&mut models, &simulated, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(
        &mut models,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let mut routes = backend.route_client();
    let created = routes
        .create_route(CreateRouteRequest {
            name: "replace-mixed-route".to_owned(),
            enabled: true,
            fallback_groups: custom_groups(&simulated.model_deployment_id),
        })
        .await
        .expect("create route")
        .into_inner()
        .route
        .expect("created route")
        .route
        .expect("created declaration");
    let fallback_groups = mixed_custom_groups(
        &simulated.model_deployment_id,
        &upstream_deployment.model_deployment_id,
    );
    let replaced = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: created.inference_route_id.clone(),
            expected_generation: created.generation,
            name: created.name.clone(),
            enabled: created.enabled,
            fallback_groups: fallback_groups.clone(),
        })
        .await
        .expect("replace route with mixed members")
        .into_inner()
        .route
        .expect("replaced route")
        .route
        .expect("replaced declaration");
    assert_eq!(replaced.inference_route_id, created.inference_route_id);
    assert_eq!(replaced.fallback_groups, fallback_groups);
    assert_eq!(replaced.generation, created.generation + 1);

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn upstream_health_churn_preserves_simulated_route_and_lifecycle() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let site_path = fixture.join("site.yaml");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    let runtime_support =
        dotcore_runtime_backends::simulated::SimulatedRuntimeTestSupport::with_stop_failures(0);
    let inference_support = dotcore::testing::SimulatedInferenceTestSupport::blocked();
    let backend = support::RunningBackend::start_with_simulated_test_supports(
        support::configuration(&fixture),
        runtime_support,
        inference_support.clone(),
    )
    .await;
    let mut model = backend.model_client();
    let simulated = model
        .create_model_deployment(local_creation_request("fixture-model".to_owned()))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let simulated_ready =
        wait_for_lifecycle(&mut model, &simulated, ModelDeploymentStatus::Ready).await;
    let upstream_deployment = model
        .create_model_deployment(upstream_creation_request(
            PUBLIC_MODEL.to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(
        &mut model,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;
    wait_for_model_ids(&backend, &["fixture-model", PUBLIC_MODEL]).await;

    let simulated_request = request_at_with_timeout(
        backend.inference_address(),
        "POST",
        "/v1/chat/completions",
        br#"{"model":"fixture-model","messages":[{"role":"user","content":"local"}]}"#,
        LEASE_CONTINUITY_TIMEOUT,
    );
    tokio::pin!(simulated_request);
    tokio::select! {
        response = &mut simulated_request => panic!("Simulated request completed before upstream health churn: {}", response.status),
        () = inference_support.wait_for_blocked_inference() => {}
    };

    upstream.fail_after_the_next_get();
    let failed_health_edit = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: upstream_deployment.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v2",
            )),
        })
        .await
        .expect("accept edit before its recurring health failure")
        .into_inner()
        .deployment
        .expect("edited upstream deployment");
    wait_for_lifecycle_with_timeout(
        &mut model,
        &failed_health_edit,
        ModelDeploymentStatus::Unavailable,
        HEALTH_TIMEOUT,
    )
    .await;
    wait_for_model_ids(&backend, &["fixture-model"]).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;

    upstream.set_outage(false);
    let recovered_health_edit = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: upstream_deployment.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v3",
            )),
        })
        .await
        .expect("accept recovery edit")
        .into_inner()
        .deployment
        .expect("recovered upstream deployment");
    wait_for_lifecycle_with_timeout(
        &mut model,
        &recovered_health_edit,
        ModelDeploymentStatus::Ready,
        HEALTH_TIMEOUT,
    )
    .await;
    wait_for_model_ids(&backend, &["fixture-model", PUBLIC_MODEL]).await;
    inference_support.release_blocked_inference();
    let simulated_response = tokio::time::timeout(READY_TIMEOUT, &mut simulated_request)
        .await
        .expect("admitted simulated request completion deadline");
    assert_eq!(simulated_response.status, 200);
    assert_eq!(json(&simulated_response)["model"], "fixture-model");
    assert_eq!(
        json(&simulated_response)["choices"][0]["message"]["content"],
        "Simulated response."
    );
    assert_simulated_serves(&backend).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn upstream_withdrawal_cancels_its_request_without_interrupting_simulated_serving() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let site_path = fixture.join("site.yaml");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();
    let simulated = model
        .create_model_deployment(local_creation_request("fixture-model".to_owned()))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    let simulated_ready =
        wait_for_lifecycle(&mut model, &simulated, ModelDeploymentStatus::Ready).await;
    let upstream_deployment = model
        .create_model_deployment(upstream_creation_request(
            PUBLIC_MODEL.to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("upstream deployment");
    wait_for_lifecycle(
        &mut model,
        &upstream_deployment,
        ModelDeploymentStatus::Ready,
    )
    .await;

    upstream.block_inference();
    let inference_attempts = upstream.inference_attempt_count();
    let inference = request_at(
        backend.inference_address(),
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"remote"}]}"#,
    );
    tokio::pin!(inference);
    tokio::select! {
        response = &mut inference => panic!("blocked upstream inference completed early: {}", response.status),
        () = upstream.wait_for_blocked_inference() => {}
    };
    assert_one_inference_attempt(&upstream, inference_attempts);

    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: upstream_deployment.model_deployment_id,
        })
        .await
        .expect("withdraw upstream model");
    upstream.wait_for_inference_cancellation().await;
    let cancelled = tokio::time::timeout(READY_TIMEOUT, &mut inference)
        .await
        .expect("cancelled inference response deadline");
    assert_ne!(
        cancelled.status, 200,
        "withdrawn upstream request must not succeed"
    );
    wait_for_model_ids(&backend, &["fixture-model"]).await;
    assert_simulated_serves(&backend).await;
    assert_simulated_unchanged(&mut model, &simulated_ready).await;
    assert_one_inference_attempt(&upstream, inference_attempts);

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn generic_endpoint_covers_composed_lifecycle_and_public_boundaries() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    let initial_configuration = endpoint_configuration(&upstream);
    let loaded = model
        .create_model_deployment(upstream_creation_request(
            PUBLIC_MODEL.to_owned(),
            initial_configuration.clone(),
        ))
        .await
        .expect("load upstream model")
        .into_inner()
        .deployment
        .expect("loaded upstream model");
    assert_management_deployment(&loaded);
    assert_eq!(
        loaded.upstream_desired_configuration.as_ref(),
        Some(&initial_configuration)
    );
    assert_owner_only(&fixture.join("site.yaml"));
    let ready = wait_for_lifecycle(&mut model, &loaded, ModelDeploymentStatus::Ready).await;
    assert_eq!(
        ready.upstream_desired_configuration.as_ref(),
        Some(&initial_configuration)
    );

    let list = request(&backend, "GET", "/v1/models", b"").await;
    assert_public_response(&list, &upstream);
    assert_eq!(list.status, 200);
    assert_eq!(
        json(&list),
        serde_json::json!({
            "object":"list", "data":[{"id":PUBLIC_MODEL,"object":"model","created":0,"owned_by":"dotcore"}]
        })
    );

    let chat = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_public_response(&chat, &upstream);
    assert_eq!(chat.status, 200);
    assert_eq!(json(&chat)["model"], PUBLIC_MODEL);
    assert_eq!(
        json(&chat)["choices"][0]["message"]["content"],
        "Scripted response."
    );

    let tools = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"scripted-model","messages":[{"role":"user","content":"use a tool"}],"tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}]}"#).await;
    assert_public_response(&tools, &upstream);
    assert_eq!(tools.status, 200);
    assert_eq!(
        json(&tools)["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );

    let stream = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"scripted-model","messages":[{"role":"user","content":"stream"}],"stream":true,"stream_options":{"include_usage":true}}"#).await;
    assert_public_response(&stream, &upstream);
    assert_eq!(stream.status, 200);
    assert_stream_response(&stream.body, PUBLIC_MODEL);

    // The upstream is always asked for usage; the client sees it only when it asked.
    let unrequested = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"scripted-model","messages":[{"role":"user","content":"stream"}],"stream":true}"#).await;
    assert_public_response(&unrequested, &upstream);
    assert_eq!(unrequested.status, 200);
    let unrequested_body = String::from_utf8(unrequested.body.clone()).expect("stream UTF-8");
    assert_eq!(unrequested_body.matches("data: ").count(), 4);
    assert!(!unrequested_body.contains("usage"));
    let upstream_stream_options = upstream
        .requests()
        .iter()
        .rev()
        .find_map(|captured| {
            serde_json::from_slice::<serde_json::Value>(&captured.body)
                .ok()
                .filter(|body| body["stream"] == true)
        })
        .expect("streamed upstream request")["stream_options"]
        .clone();
    assert_eq!(
        upstream_stream_options,
        serde_json::json!({"include_usage": true})
    );

    let responses = request(
        &backend,
        "POST",
        "/v1/responses",
        br#"{"model":"scripted-model","store":false,"input":"hello"}"#,
    )
    .await;
    assert_public_response(&responses, &upstream);
    assert_eq!(responses.status, 200);
    assert_eq!(json(&responses)["model"], PUBLIC_MODEL);
    assert_eq!(
        json(&responses)["output"][0]["content"][0]["text"],
        "Scripted response."
    );

    let image = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"scripted-model","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=","detail":"auto"}}]}]}"#).await;
    assert_public_response(&image, &upstream);
    assert_eq!(image.status, 200);

    let replacement = endpoint_configuration_for_model(&upstream, "upstream-private-model-v2");
    let edited = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
            upstream_desired_configuration: Some(replacement.clone()),
        })
        .await
        .expect("edit upstream model")
        .into_inner()
        .deployment
        .expect("edited upstream model");
    assert_management_deployment(&edited);
    assert_eq!(
        edited.upstream_desired_configuration.as_ref(),
        Some(&replacement)
    );
    assert_owner_only(&fixture.join("site.yaml"));
    wait_for_lifecycle(&mut model, &edited, ModelDeploymentStatus::Ready).await;

    upstream.set_outage(true);
    let failed = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"outage"}]}"#,
    )
    .await;
    assert_public_response(&failed, &upstream);
    assert_eq!(failed.status, 502);
    assert_eq!(json(&failed)["error"]["type"], "upstream_error");
    let failed_update = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v3",
            )),
        })
        .await
        .expect_err("outage must reject an unverified replacement");
    assert_eq!(failed_update.code(), tonic::Code::FailedPrecondition);
    upstream.set_outage(false);
    let current = model
        .list_model_deployments(dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {})
        .await
        .expect("deployment list after rejected update")
        .into_inner()
        .deployments;
    assert_eq!(current.len(), 1);
    assert_eq!(
        current[0].upstream_desired_configuration.as_ref(),
        Some(&replacement),
        "a rejected candidate must not replace the active configuration"
    );
    assert_eq!(
        json(&request(&backend, "GET", "/v1/models", b"").await)["data"],
        serde_json::json!([{"id":PUBLIC_MODEL,"object":"model","created":0,"owned_by":"dotcore"}])
    );
    let recovered = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"recovered"}]}"#,
    )
    .await;
    assert_public_response(&recovered, &upstream);
    assert_eq!(recovered.status, 200);
    assert_eq!(json(&recovered)["model"], PUBLIC_MODEL);

    let unloaded = model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("unload upstream model")
        .into_inner()
        .deployment
        .expect("unloaded upstream model");
    assert_management_deployment(&unloaded);
    wait_for_removed(&mut model).await;
    assert_eq!(
        request(&backend, "GET", "/v1/models", b"").await.status,
        200
    );
    assert!(
        json(&request(&backend, "GET", "/v1/models", b"").await)["data"]
            .as_array()
            .expect("models")
            .is_empty()
    );

    let requests = upstream.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "GET" && request.path == "/v1/models")
            .count(),
        5
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        8
    );
    assert!(requests.iter().all(
        |request| request.headers.get("authorization") == Some(&format!("Bearer {CREDENTIAL}"))
    ));
    assert!(requests.iter().any(|request| {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|body| {
                body.get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            == Some(PRIVATE_MODEL.to_owned())
    }));
    assert!(requests.iter().any(|request| {
        serde_json::from_slice::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|body| {
                body.get("model")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            == Some("upstream-private-model-v2".to_owned())
    }));
    assert_public_bytes(
        &serde_json::to_string(
            &requests
                .iter()
                .map(|request| &request.body)
                .collect::<Vec<_>>(),
        )
        .expect("captured payloads"),
    );
    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn generic_endpoint_rejects_failed_loads_and_dispatches_without_a_key() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    upstream.set_outage(true);
    let failed_load = model
        .create_model_deployment(upstream_creation_request(
            "failed-model".to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect_err("unreachable endpoint must reject a new deployment");
    assert_eq!(failed_load.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        failed_load.message(),
        format!(
            "Upstream endpoint preflight failed: The endpoint rejected the verification request; request URL: {}/models; HTTP status: 503; response: {{\"error\":\"scripted outage\"}}",
            upstream.base_url(),
        )
    );
    assert!(!failed_load.message().contains(['\n', '\r']));
    assert!(failed_load.message().len() <= 2_048);
    assert_eq!(failed_load.details(), b"");
    assert!(
        model
            .list_model_deployments(
                dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {}
            )
            .await
            .expect("deployment list after failed load")
            .into_inner()
            .deployments
            .is_empty(),
        "a failed load must not create a declaration"
    );
    assert_eq!(
        json(&request(&backend, "GET", "/v1/models", b"").await),
        serde_json::json!({"object":"list", "data":[]})
    );
    assert_eq!(
        backend
            .route_client()
            .list_routes(ListRoutesRequest {})
            .await
            .expect("route list after failed load")
            .into_inner()
            .routes,
        vec![],
        "a failed load must not create a exposure route"
    );
    assert_eq!(
        upstream.inference_attempt_count(),
        0,
        "endpoint preflight must not send inference requests"
    );

    upstream.set_outage(false);
    let mut keyless_configuration = endpoint_configuration(&upstream);
    keyless_configuration.api_key.clear();
    let loaded = model
        .create_model_deployment(upstream_creation_request(
            "keyless-model".to_owned(),
            keyless_configuration,
        ))
        .await
        .expect("keyless upstream load")
        .into_inner()
        .deployment
        .expect("keyless deployment");
    wait_for_lifecycle(&mut model, &loaded, ModelDeploymentStatus::Ready).await;
    let preflight = upstream.requests().pop().expect("keyless preflight");
    assert_eq!(preflight.method, "GET");
    assert_eq!(preflight.path, "/v1/models");
    assert!(
        !preflight.headers.contains_key("authorization"),
        "keyless preflight must not send authorization"
    );

    let inference = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"keyless-model","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_public_response(&inference, &upstream);
    assert_eq!(inference.status, 200);
    let dispatched = upstream.requests().pop().expect("keyless inference");
    assert_eq!(dispatched.method, "POST");
    assert_eq!(dispatched.path, "/v1/chat/completions");
    assert!(
        !dispatched.headers.contains_key("authorization"),
        "keyless inference must not send authorization"
    );

    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload keyless deployment");
    wait_for_removed(&mut model).await;
    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn invalid_raw_multibyte_endpoint_url_is_normalized_and_truncated_at_utf8_boundaries() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();
    let mut configuration = endpoint_configuration(&upstream);
    configuration.endpoint.as_mut().expect("endpoint").base_url =
        format!("{} \n\r\t{}", "é".repeat(190), "é".repeat(210));

    let failed = model
        .create_model_deployment(upstream_creation_request(
            "invalid-multibyte-url".to_owned(),
            configuration,
        ))
        .await
        .expect_err("invalid endpoint must reject a new deployment");
    let expected_url = format!("{} {}…", "é".repeat(190), "é".repeat(192));

    assert_eq!(failed.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        failed.message(),
        format!(
            "Upstream endpoint preflight failed: The endpoint configuration is invalid; request URL: {expected_url}; cause: The upstream endpoint URL is invalid."
        )
    );
    assert!(!failed.message().contains(['\n', '\r', '\t']));
    assert!(failed.message().len() <= 2_048);
    assert_eq!(failed.details(), b"");
    assert!(
        model
            .list_model_deployments(
                dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {}
            )
            .await
            .expect("deployment list after invalid endpoint")
            .into_inner()
            .deployments
            .is_empty()
    );

    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn transport_failure_returns_plain_preflight_message_without_accepting_state() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let endpoint = endpoint_configuration(&upstream);
    let expected_request_url = format!("{}/models", upstream.base_url());
    upstream.stop().await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    let failed = model
        .create_model_deployment(upstream_creation_request(
            "offline-model".to_owned(),
            endpoint,
        ))
        .await
        .expect_err("offline endpoint must reject a new deployment");

    assert_eq!(failed.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        failed.message(),
        format!(
            "Upstream endpoint preflight failed: The endpoint could not be reached; request URL: {expected_request_url}; cause: network connection failed"
        )
    );
    assert!(!failed.message().contains(['\n', '\r']));
    assert!(failed.message().len() <= 2_048);
    assert_eq!(failed.details(), b"");
    assert_eq!(
        model
            .list_model_deployments(
                dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {}
            )
            .await
            .expect("deployment list after transport failure")
            .into_inner()
            .deployments,
        vec![]
    );
    assert_eq!(
        backend
            .route_client()
            .list_routes(ListRoutesRequest {})
            .await
            .expect("route list after transport failure")
            .into_inner()
            .routes,
        vec![]
    );

    backend.stop().await;
}

#[tokio::test]
async fn generic_endpoint_restart_direct_yaml_and_permissions_use_fresh_preflight() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let configuration = support::configuration(&fixture);
    let backend = support::RunningBackend::start(configuration.clone()).await;
    let mut model = backend.model_client();
    let loaded = model
        .create_model_deployment(upstream_creation_request(
            "restart-model".to_owned(),
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("generic endpoint load")
        .into_inner()
        .deployment
        .expect("generic endpoint deployment");
    assert_management_deployment(&loaded);
    wait_for_lifecycle(&mut model, &loaded, ModelDeploymentStatus::Ready).await;
    let initial_probe_count = upstream
        .requests()
        .iter()
        .filter(|request| request.method == "GET")
        .count();
    assert_eq!(initial_probe_count, 2);
    assert!(
        upstream
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .all(|request| request.path == "/v1/models")
    );
    assert_eq!(
        json(&request(&backend, "GET", "/v1/models", b"").await),
        serde_json::json!({
            "object":"list", "data":[{"id":"restart-model","object":"model","created":0,"owned_by":"dotcore"}]
        })
    );
    let chat_attempts = upstream.inference_attempt_count();
    let chat = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"restart-model","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_eq!(chat.status, 200);
    assert_eq!(json(&chat)["model"], "restart-model");
    assert_eq!(
        json(&chat)["choices"][0]["message"]["content"],
        "Scripted response."
    );
    assert_one_inference_attempt(&upstream, chat_attempts);

    let tools_attempts = upstream.inference_attempt_count();
    let tools = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"restart-model","messages":[{"role":"user","content":"use a tool"}],"tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}]}"#).await;
    assert_eq!(tools.status, 200);
    assert_eq!(
        json(&tools)["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert_one_inference_attempt(&upstream, tools_attempts);

    let stream_attempts = upstream.inference_attempt_count();
    let stream = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"restart-model","messages":[{"role":"user","content":"stream"}],"stream":true,"stream_options":{"include_usage":true}}"#).await;
    assert_eq!(stream.status, 200);
    assert_stream_response(&stream.body, "restart-model");
    assert_one_inference_attempt(&upstream, stream_attempts);

    let responses_attempts = upstream.inference_attempt_count();
    let responses = request(
        &backend,
        "POST",
        "/v1/responses",
        br#"{"model":"restart-model","store":false,"input":"hello"}"#,
    )
    .await;
    assert_eq!(responses.status, 200);
    assert_eq!(json(&responses)["model"], "restart-model");
    assert_eq!(
        json(&responses)["output"][0]["content"][0]["text"],
        "Scripted response."
    );
    assert_one_inference_attempt(&upstream, responses_attempts);

    let image_attempts = upstream.inference_attempt_count();
    let image = request(&backend, "POST", "/v1/chat/completions", br#"{"model":"restart-model","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=","detail":"auto"}}]}]}"#).await;
    assert_eq!(image.status, 200);
    assert_one_inference_attempt(&upstream, image_attempts);

    let updated = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v2",
            )),
        })
        .await
        .expect("generic endpoint edit")
        .into_inner()
        .deployment
        .expect("generic endpoint edited deployment");
    wait_for_lifecycle(&mut model, &updated, ModelDeploymentStatus::Ready).await;

    upstream.set_outage(true);
    let failed_update = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
            upstream_desired_configuration: Some(endpoint_configuration_for_model(
                &upstream,
                "upstream-private-model-v3",
            )),
        })
        .await
        .expect_err("outage must reject an unverified replacement");
    assert_eq!(failed_update.code(), tonic::Code::FailedPrecondition);
    upstream.set_outage(false);

    let persisted = fs::read_to_string(fixture.join("site.yaml")).expect("persisted site");
    assert!(persisted.contains("fixture-secret-do-not-log"));
    assert_owner_only(&fixture.join("site.yaml"));
    let probe_count_before_restart = upstream
        .requests()
        .iter()
        .filter(|request| request.method == "GET")
        .count();
    backend.stop().await;
    wait_for_state_release(&fixture).await;

    upstream.set_outage(true);
    // The restarted backend retries a failed verification only when the test advances its clock
    // past the production retry delay, so no retry probe lands between the exact counts.
    let retry = Timing::production().model.upstream_retry;
    let timing = Timing {
        model: ModelTiming {
            upstream_retry: retry,
            ..Timing::test().model
        },
        ..Timing::test()
    };
    timing.check().expect("restart timing");
    let clock = ManualClock::new();
    let restarted = support::RunningBackend::start_with_test_support(
        configuration.clone(),
        BackendTestSupport {
            timing,
            clock: Some(Arc::new(clock.clone())),
            ..BackendTestSupport::test()
        },
    )
    .await;
    let mut restarted_model = restarted.model_client();
    // The retained record reads Ready until the fresh preflight fails, so Unavailable proves
    // the probe ran and was classified before the served model list is checked.
    wait_for_lifecycle(
        &mut restarted_model,
        &loaded,
        ModelDeploymentStatus::Unavailable,
    )
    .await;
    assert_eq!(
        json(&request(&restarted, "GET", "/v1/models", b"").await),
        serde_json::json!({"object":"list", "data":[]})
    );
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        probe_count_before_restart + 1
    );
    upstream.set_outage(false);
    clock.advance(retry.initial);
    let recovered = wait_for_lifecycle_with_timeout(
        &mut restarted_model,
        &loaded,
        ModelDeploymentStatus::Ready,
        RECOVERY_TIMEOUT,
    )
    .await;
    assert_eq!(recovered.model_deployment_id, loaded.model_deployment_id);
    assert_eq!(
        upstream
            .requests()
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        probe_count_before_restart + 2
    );
    let unloaded = restarted_model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload recovered upstream model")
        .into_inner()
        .deployment
        .expect("unloaded recovered upstream model");
    assert_management_deployment(&unloaded);
    wait_for_removed(&mut restarted_model).await;
    restarted.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn direct_yaml_upstream_starts_without_site_init_or_runtime_discovery() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let site_path = fixture.join("site.yaml");
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("Site");
    site.model_deployments.push(direct_deployment(&upstream));
    site.inference_routes
        .push(direct_route("yaml-model", "deployment-1"));
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("site YAML"),
    )
    .expect("direct YAML");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();
    let deployment = wait_for_lifecycle(
        &mut model,
        &ModelDeployment {
            model_deployment_id: "deployment-1".to_owned(),
            ..Default::default()
        },
        ModelDeploymentStatus::Ready,
    )
    .await;
    assert_eq!(deployment.model_deployment_id, "deployment-1");
    assert_eq!(
        request(&backend, "GET", "/v1/models", b"").await.status,
        200
    );
    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        })
        .await
        .expect("unload YAML model");
    wait_for_removed(&mut model).await;
    backend.stop().await;
    upstream.stop().await;
}

#[tokio::test]
async fn direct_yaml_rejects_a_simulated_and_upstream_duplicate_route() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = support::temporary_fast_fixture();
    let configuration = support::configuration(&fixture);
    let backend = support::RunningBackend::start(configuration.clone()).await;
    let mut model = backend.model_client();
    let simulated = model
        .create_model_deployment(local_creation_request("fixture-model".to_owned()))
        .await
        .expect("load simulated model")
        .into_inner()
        .deployment
        .expect("simulated deployment");
    wait_for_lifecycle(&mut model, &simulated, ModelDeploymentStatus::Ready).await;
    backend.stop().await;
    wait_for_state_release(&fixture).await;

    let site_path = fixture.join("site.yaml");
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("Site");
    site.model_deployments.push(direct_deployment(&upstream));
    site.inference_routes
        .push(direct_route("fixture-model", "deployment-1"));
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("duplicate site YAML"),
    )
    .expect("duplicate site YAML");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");

    let result = support::prepare_backend(configuration, std::future::pending()).await;
    let error = match result {
        Ok(_) => panic!("duplicate direct YAML site must not compose"),
        Err(error) => error,
    };
    assert!(
        error.to_string().contains("desired site failure"),
        "unexpected duplicate direct-YAML error: {error:#}"
    );
    assert!(
        upstream.requests().is_empty(),
        "invalid YAML must not probe upstream"
    );

    upstream.stop().await;
}

#[tokio::test]
async fn startup_rejects_over_permissive_credential_bearing_yaml() {
    install_credential_guard();
    use std::os::unix::fs::PermissionsExt;
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let site_path = fixture.join("site.yaml");
    let mut site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("Site");
    site.model_deployments.push(direct_deployment(&upstream));
    site.inference_routes
        .push(direct_route("unsafe-model", "deployment-1"));
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("site YAML"),
    )
    .expect("site YAML");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o644)).expect("permissive mode");
    let error = match dotcore::composition::Backend::prepare(
        dotcore::composition::BackendConfiguration {
            site_paths: support::configuration(&fixture),
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: "127.0.0.1:0".parse().expect("management"),
                    inference_bind_address: "127.0.0.1:0".parse().expect("inference"),
                    private_listener: dotcore::composition::ManagerPrivateListener::SiteDefault {
                        port: 0,
                    },
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().expect("origin")],
                },
            ),
        },
        async {},
    )
    .await
    {
        Ok(_) => panic!("permissive credential file must be rejected"),
        Err(error) => error,
    };
    assert!(format!("{:#}", anyhow::Error::from(error)).contains("chmod 600"));
    upstream.stop().await;
}

#[tokio::test]
async fn ready_upstream_metadata_update_preserves_dispatch_without_preflight_and_endpoint_edit_repreflights()
 {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();
    let created = model
        .create_model_deployment(upstream_creation_request(
            "scripted-model",
            endpoint_configuration(&upstream),
        ))
        .await
        .expect("create upstream deployment")
        .into_inner()
        .deployment
        .expect("created upstream deployment");
    let ready = wait_for_lifecycle(&mut model, &created, ModelDeploymentStatus::Ready).await;
    let requests_before_metadata_update = upstream.requests();

    let renamed = model
        .update_model_deployment_metadata(UpdateModelDeploymentMetadataRequest {
            model_deployment_id: created.model_deployment_id.clone(),
            metadata: Some(ModelDeploymentMetadata {
                display_name: Some("Ready upstream metadata update".to_owned()),
            }),
            update_mask: Some(FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        })
        .await
        .expect("update ready upstream deployment")
        .into_inner()
        .deployment
        .expect("renamed upstream deployment");
    assert_eq!(
        renamed.model_deployment_id, created.model_deployment_id,
        "rename preserves immutable target identity"
    );
    assert_eq!(
        renamed.model_deployment_name,
        "Ready upstream metadata update"
    );
    assert_eq!(renamed.state, ready.state);
    assert_eq!(
        renamed.upstream_desired_configuration,
        ready.upstream_desired_configuration
    );
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        upstream.requests(),
        requests_before_metadata_update,
        "metadata updates must not repeat endpoint preflight or alter upstream dispatch"
    );

    let dispatched = request(
        &backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"scripted-model","messages":[{"role":"user","content":"rename"}]}"#,
    )
    .await;
    assert_public_response(&dispatched, &upstream);
    assert_eq!(dispatched.status, 200);
    assert_eq!(json(&dispatched)["model"], "scripted-model");

    let requests_before_edit = upstream.requests();
    let mut replacement = endpoint_configuration_for_model(&upstream, "upstream-private-model-v2");
    replacement
        .endpoint
        .as_mut()
        .expect("replacement endpoint")
        .base_url = format!("{}/alternate", upstream.base_url());
    let edited = model
        .update_model_deployment(UpdateModelDeploymentRequest {
            model_deployment_id: created.model_deployment_id.clone(),
            upstream_desired_configuration: Some(replacement),
        })
        .await
        .expect("edit upstream endpoint configuration")
        .into_inner()
        .deployment
        .expect("edited upstream deployment");
    assert_ne!(
        edited.upstream_desired_configuration,
        renamed.upstream_desired_configuration
    );
    let requests_after_edit = upstream.requests();
    assert_eq!(requests_after_edit.len(), requests_before_edit.len() + 1);
    assert_eq!(
        requests_after_edit.last().expect("edit preflight").method,
        "GET"
    );
    assert_eq!(
        requests_after_edit.last().expect("edit preflight").path,
        "/v1/alternate/models"
    );

    backend.stop().await;
    upstream.stop().await;
}

fn local_creation_request(model_id: String) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(
            dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                    model_id: model_id.clone(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            ),
        ),
        exposure: Some(direct_exposure(model_id)),
        model_deployment_name: None,
    }
}

fn hidden_local_creation_request(model_id: &str) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(
            dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                    model_id: model_id.to_owned(),
                    profile_id: "fixture-profile".to_owned(),
                    placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                    configuration: Some(support::local_model_configuration(
                        "fixture-artifact",
                        32_768,
                    )),
                },
            ),
        ),
        exposure: Some(hidden_exposure()),
        model_deployment_name: None,
    }
}

fn upstream_creation_request(
    route_name: impl Into<String>,
    desired_configuration: UpstreamModelDesiredConfiguration,
) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(
            dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Upstream(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(desired_configuration),
                },
            ),
        ),
        exposure: Some(direct_exposure(route_name)),
        model_deployment_name: None,
    }
}

fn hidden_upstream_creation_request(
    _model_id: &str,
    desired_configuration: UpstreamModelDesiredConfiguration,
) -> CreateModelDeploymentRequest {
    CreateModelDeploymentRequest {
        source: Some(
            dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Upstream(
                CreateUpstreamModelDeploymentRequest {
                    upstream_model_template_id: "openai-compatible".to_owned(),
                    desired_configuration: Some(desired_configuration),
                },
            ),
        ),
        exposure: Some(hidden_exposure()),
        model_deployment_name: None,
    }
}

fn mixed_custom_groups(
    simulated_deployment_id: &str,
    upstream_deployment_id: &str,
) -> Vec<FallbackGroup> {
    vec![
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![simulated_deployment_id.to_owned()],
        },
        FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![upstream_deployment_id.to_owned()],
        },
    ]
}

fn custom_groups(model_deployment_id: &str) -> Vec<FallbackGroup> {
    vec![FallbackGroup {
        selection: FallbackGroupSelection::Single as i32,
        model_deployment_ids: vec![model_deployment_id.to_owned()],
    }]
}

fn direct_exposure(route_name: impl Into<String>) -> ModelDeploymentExposure {
    ModelDeploymentExposure {
        kind: Some(model_deployment_exposure::Kind::Direct(
            DirectRouteExposure {
                route_name: route_name.into(),
            },
        )),
    }
}

#[tokio::test]
async fn deployments_with_shared_provider_model_id_dispatch_to_their_own_endpoints() {
    install_credential_guard();
    let first_upstream = ScriptedUpstream::start().await;
    let second_upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&first_upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();

    let first = model
        .create_model_deployment(upstream_creation_request(
            "first-route",
            endpoint_configuration(&first_upstream),
        ))
        .await
        .expect("first deployment")
        .into_inner()
        .deployment
        .expect("first deployment record");
    let second = model
        .create_model_deployment(upstream_creation_request(
            "second-route",
            endpoint_configuration(&second_upstream),
        ))
        .await
        .expect("second deployment with the same provider model ID")
        .into_inner()
        .deployment
        .expect("second deployment record");
    assert_ne!(first.model_deployment_id, second.model_deployment_id);
    wait_for_lifecycle(&mut model, &first, ModelDeploymentStatus::Ready).await;
    wait_for_lifecycle(&mut model, &second, ModelDeploymentStatus::Ready).await;

    let first_attempts = first_upstream.inference_attempt_count();
    let second_attempts = second_upstream.inference_attempt_count();
    assert_eq!(
        request(
            &backend,
            "POST",
            "/v1/chat/completions",
            br#"{"model":"first-route","messages":[{"role":"user","content":"first"}]}"#,
        )
        .await
        .status,
        200
    );
    assert_one_inference_attempt(&first_upstream, first_attempts);
    assert_eq!(second_upstream.inference_attempt_count(), second_attempts);
    assert_eq!(
        request(
            &backend,
            "POST",
            "/v1/chat/completions",
            br#"{"model":"second-route","messages":[{"role":"user","content":"second"}]}"#,
        )
        .await
        .status,
        200
    );
    assert_one_inference_attempt(&second_upstream, second_attempts);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &first_upstream
                .requests()
                .pop()
                .expect("first dispatch")
                .body,
        )
        .expect("first dispatch payload")["model"],
        PRIVATE_MODEL
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &second_upstream
                .requests()
                .pop()
                .expect("second dispatch")
                .body,
        )
        .expect("second dispatch payload")["model"],
        PRIVATE_MODEL
    );

    backend.stop().await;
    first_upstream.stop().await;
    second_upstream.stop().await;
}

#[tokio::test]
async fn hidden_upstream_deployment_creates_no_route() {
    install_credential_guard();
    let upstream = ScriptedUpstream::start().await;
    let fixture = upstream_fixture(&upstream).await;
    let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
    let mut model = backend.model_client();
    let mut routes = backend.route_client();
    let mut deployment_request =
        upstream_creation_request("unused-route", endpoint_configuration(&upstream));
    deployment_request.exposure = Some(hidden_exposure());
    let deployment = model
        .create_model_deployment(deployment_request)
        .await
        .expect("hidden upstream deployment")
        .into_inner()
        .deployment
        .expect("hidden deployment record");
    wait_for_lifecycle(&mut model, &deployment, ModelDeploymentStatus::Ready).await;

    assert_eq!(
        routes
            .list_routes(ListRoutesRequest {})
            .await
            .expect("route list")
            .into_inner()
            .routes,
        []
    );
    assert_eq!(
        json(&request(&backend, "GET", "/v1/models", b"").await),
        serde_json::json!({"object":"list", "data":[]})
    );

    model
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        })
        .await
        .expect("remove hidden deployment");
    wait_for_removed(&mut model).await;
    backend.stop().await;
    upstream.stop().await;
}

fn hidden_exposure() -> ModelDeploymentExposure {
    ModelDeploymentExposure {
        kind: Some(model_deployment_exposure::Kind::Hidden(Default::default())),
    }
}

fn endpoint_configuration(upstream: &ScriptedUpstream) -> UpstreamModelDesiredConfiguration {
    endpoint_configuration_for_model(upstream, PRIVATE_MODEL)
}

fn endpoint_configuration_for_model(
    upstream: &ScriptedUpstream,
    model: &str,
) -> UpstreamModelDesiredConfiguration {
    UpstreamModelDesiredConfiguration {
        upstream_model_id: model.to_owned(),
        api_key: CREDENTIAL.to_owned(),
        input_capabilities: Some(UpstreamModelInputCapabilities {
            supports_text: true,
            supports_image: true,
        }),
        endpoint: Some(OpenAiCompatibleEndpointConfiguration {
            base_url: upstream.base_url(),
            allow_insecure_http: true,
        }),
        boundary: Some(UpstreamModelBoundary {
            locality: UpstreamModelLocality::Private as i32,
            trust_domain: "scripted.test".to_owned(),
            data_egress_class: UpstreamModelDataEgressClass::PrivateNetwork as i32,
        }),
    }
}

fn direct_deployment(
    upstream: &ScriptedUpstream,
) -> dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
    let template = dotcore_catalog::test_support::openai_compatible_template();
    dotcore_proto::dotcore::model::v1::ModelDeploymentDeclaration {
        model_deployment_id: "deployment-1".to_owned(),
        snapshot: Some(dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot {
            source: Some(
                dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Upstream(
                    dotcore_proto::dotcore::model::v1::UpstreamModelDeploymentSnapshot {
                        upstream_model_template: Some(template),
                    },
                ),
            ),
        }),
        desired_state: dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present
            as i32,
        upstream_desired_configuration: Some(endpoint_configuration(upstream)),
        lifecycle_generation: 1,
        lifecycle_transition:
            dotcore_proto::dotcore::model::v1::ModelDeploymentLifecycleTransition::Create as i32,
        ..Default::default()
    }
}

fn direct_route(route_name: &str, model_deployment_id: &str) -> InferenceRoute {
    InferenceRoute {
        inference_route_id: format!("route-{route_name}"),
        name: route_name.to_owned(),
        enabled: true,
        fallback_groups: vec![FallbackGroup {
            selection: FallbackGroupSelection::Single as i32,
            model_deployment_ids: vec![model_deployment_id.to_owned()],
        }],
        generation: 1,
    }
}

async fn upstream_fixture(_upstream: &ScriptedUpstream) -> support::TemporaryFixture {
    let fixture = support::temporary_fixture();
    let site_path = fixture.join("site.yaml");
    let site: dotcore_proto::dotcore::site::v1::Site =
        dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
    let mut site = site;
    site.execution_targets.clear();
    site.model_deployments = vec![];
    fs::write(
        &site_path,
        dotcore_proto::yaml::to_string(&site).expect("upstream site YAML"),
    )
    .expect("upstream site fixture");
    fs::set_permissions(&site_path, fs::Permissions::from_mode(0o600)).expect("Site permissions");
    fixture
}

fn assert_one_inference_attempt(upstream: &ScriptedUpstream, attempts_before: usize) {
    assert_eq!(upstream.inference_attempt_count(), attempts_before + 1);
}

fn assert_stream_response(body: &[u8], public_model: &str) {
    let stream = String::from_utf8(body.to_vec()).expect("upstream stream UTF-8");
    let events = stream
        .split("\n\n")
        .filter(|event| !event.is_empty())
        .map(|event| event.strip_prefix("data: ").expect("SSE data frame"))
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 5);
    assert_eq!(events[4], "[DONE]");

    let chunks = events[..4]
        .iter()
        .map(|event| serde_json::from_str::<serde_json::Value>(event).expect("SSE JSON frame"))
        .collect::<Vec<_>>();
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["model"] == serde_json::Value::String(public_model.to_owned()))
    );
    assert_eq!(
        chunks[1]["choices"][0]["delta"]["content"],
        "Scripted response."
    );
    assert_eq!(chunks[2]["choices"][0]["finish_reason"], "stop");
    assert_eq!(
        chunks[3]["usage"],
        serde_json::json!({"prompt_tokens":1,"completion_tokens":1,"total_tokens":2})
    );
}

fn assert_management_deployment(deployment: &ModelDeployment) {
    let configuration = deployment
        .upstream_desired_configuration
        .as_ref()
        .expect("trusted POC upstream configuration");
    assert_eq!(configuration.api_key, CREDENTIAL);
    assert_eq!(deployment.model_deployment_id, "model-d44bc5d823b11591-1");
    assert!(matches!(
        deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref()),
        Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Upstream(_))
    ));
}

fn assert_public_bytes(value: &str) {
    assert!(
        !value.contains(CREDENTIAL),
        "credential leaked into public data: {value}"
    );
    assert!(
        !value.contains("upstream-private-model"),
        "private upstream model leaked into public data: {value}"
    );
}

fn assert_public_response(response: &HttpResponse, upstream: &ScriptedUpstream) {
    let body = String::from_utf8_lossy(&response.body);
    assert_public_bytes(&body);
    assert!(
        !body.contains(&upstream.base_url()),
        "private upstream endpoint leaked into public data: {body}"
    );
}

fn assert_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(path)
            .expect("Site metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

type ModelClient = dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient<
    tonic::transport::Channel,
>;

async fn wait_for_lifecycle(
    client: &mut ModelClient,
    deployment: &ModelDeployment,
    lifecycle: ModelDeploymentStatus,
) -> ModelDeployment {
    wait_for_lifecycle_with_timeout(client, deployment, lifecycle, READY_TIMEOUT).await
}

async fn wait_for_lifecycle_with_timeout(
    client: &mut ModelClient,
    deployment: &ModelDeployment,
    lifecycle: ModelDeploymentStatus,
    timeout: Duration,
) -> ModelDeployment {
    let started = std::time::Instant::now();
    let mut last_observed = None;
    let result = tokio::time::timeout(timeout, async {
        loop {
            let deployments = client
                .list_model_deployments(
                    dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {},
                )
                .await
                .expect("deployment list")
                .into_inner()
                .deployments;
            if let Some(found) = deployments
                .into_iter()
                .find(|candidate| candidate.model_deployment_id == deployment.model_deployment_id)
            {
                let current = found.state.as_ref().map(|state| state.status);
                if current == Some(lifecycle as i32) {
                    return found;
                }
                last_observed = current;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    result.unwrap_or_else(|_| {
        let observed = last_observed
            .map(|status| {
                ModelDeploymentStatus::try_from(status)
                    .map(|status| format!("{status:?}"))
                    .unwrap_or_else(|_| format!("unknown({status})"))
            })
            .unwrap_or_else(|| "no deployment state".to_owned());
        panic!(
            "upstream lifecycle timeout after {:?}: expected {:?}, deployment {}, observed {}",
            started.elapsed(),
            lifecycle,
            deployment.model_deployment_id,
            observed
        )
    })
}

async fn assert_simulated_unchanged(client: &mut ModelClient, expected: &ModelDeployment) {
    let current = wait_for_lifecycle(client, expected, ModelDeploymentStatus::Ready).await;
    assert_eq!(current.model_deployment_id, expected.model_deployment_id);
    assert_eq!(current.snapshot, expected.snapshot);
    assert_eq!(current.desired_state, expected.desired_state);
}

async fn assert_simulated_serves(backend: &support::RunningBackend) {
    let response = request(
        backend,
        "POST",
        "/v1/chat/completions",
        br#"{"model":"fixture-model","messages":[{"role":"user","content":"local"}]}"#,
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["model"], "fixture-model");
    assert_eq!(
        json(&response)["choices"][0]["message"]["content"],
        "Simulated response."
    );
}

async fn wait_for_model_ids(backend: &support::RunningBackend, expected: &[&str]) {
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            let response = request(backend, "GET", "/v1/models", b"").await;
            let body = json(&response);
            let actual = body["data"]
                .as_array()
                .expect("models data")
                .iter()
                .map(|model| model["id"].as_str().expect("model ID"))
                .collect::<Vec<_>>();
            if actual == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("served model publication timeout");
}

/// Waits until the named route shows every member eligible and returns that route listing. Route
/// availability follows serving targets, which are published after a deployment reports ready.
async fn wait_for_eligible_route_members(
    backend: &support::RunningBackend,
    route_name: &str,
) -> Vec<InferenceRouteView> {
    let mut routes = backend.route_client();
    let mut listed = Vec::new();
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            listed = routes
                .list_routes(ListRoutesRequest {})
                .await
                .expect("route list")
                .into_inner()
                .routes;
            let all_eligible = listed
                .iter()
                .find(|view| {
                    view.route
                        .as_ref()
                        .is_some_and(|route| route.name == route_name)
                })
                .and_then(|view| view.availability.as_ref())
                .is_some_and(|availability| {
                    availability
                        .fallback_groups
                        .iter()
                        .flat_map(|group| &group.members)
                        .all(|member| member.eligible)
                });
            if all_eligible {
                return listed.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("route {route_name} with every member eligible; last routes: {listed:?}")
    })
}

async fn wait_for_removed(client: &mut ModelClient) {
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            if client
                .list_model_deployments(
                    dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {},
                )
                .await
                .expect("deployment list while waiting for unload")
                .into_inner()
                .deployments
                .is_empty()
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("upstream unload timeout");
}

async fn wait_for_state_release(fixture: &Path) {
    tokio::time::timeout(READY_TIMEOUT, async {
        loop {
            match dotcore::composition::model_service_for(&support::configuration(fixture)).await {
                Ok(service) => {
                    drop(service);
                    return;
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("state release timeout");
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

async fn request(
    backend: &support::RunningBackend,
    method: &str,
    path: &str,
    body: &[u8],
) -> HttpResponse {
    request_at(backend.inference_address(), method, path, body).await
}

async fn request_at(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
) -> HttpResponse {
    request_at_with_timeout(address, method, path, body, READY_TIMEOUT).await
}

async fn request_at_with_timeout(
    address: std::net::SocketAddr,
    method: &str,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> HttpResponse {
    let mut stream = TcpStream::connect(address)
        .await
        .expect("inference connection");
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("inference request");
    stream.write_all(body).await.expect("inference body");
    let mut bytes = Vec::new();
    tokio::time::timeout(timeout, stream.read_to_end(&mut bytes))
        .await
        .expect("inference response timeout")
        .expect("inference response");
    parse_response(&bytes)
}

async fn wait_for_signal(signal: &AtomicBool, notification: &Notify, description: &str) {
    tokio::time::timeout(READY_TIMEOUT, async {
        while !signal.load(Ordering::SeqCst) {
            notification.notified().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{description} deadline"));
}

async fn serve_request(mut stream: TcpStream, state: UpstreamState) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("upstream request read");
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let length = headers
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + length {
                let request = parse_upstream_request(&bytes[..header_end + length]);
                state
                    .requests
                    .lock()
                    .expect("upstream requests")
                    .push(request.clone());
                if request.method == "POST" && state.block_inference.load(Ordering::SeqCst) {
                    state.inference_started.store(true, Ordering::SeqCst);
                    state.inference_started_signal.notify_waiters();
                    let mut cancellation = [0_u8; 1];
                    let _ = stream.read(&mut cancellation).await;
                    state.inference_cancelled.store(true, Ordering::SeqCst);
                    state.inference_cancelled_signal.notify_waiters();
                    return;
                }
                let get_count = state
                    .requests
                    .lock()
                    .expect("upstream requests")
                    .iter()
                    .filter(|request| request.method == "GET")
                    .count();
                let outage = *state.outage.lock().expect("upstream outage")
                    || (request.method == "GET"
                        && state
                            .outage_after_get_count
                            .lock()
                            .expect("upstream delayed outage")
                            .is_some_and(|after| get_count > after));
                respond(&mut stream, request, outage).await;
                return;
            }
        }
    }
}

fn parse_upstream_request(bytes: &[u8]) -> CapturedRequest {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("upstream headers");
    let header = String::from_utf8_lossy(&bytes[..end]);
    let mut lines = header.lines();
    let first = lines.next().expect("upstream request line");
    let mut parts = first.split_whitespace();
    let method = parts.next().expect("upstream method").to_owned();
    let path = parts.next().expect("upstream path").to_owned();
    let headers = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    CapturedRequest {
        method,
        path,
        headers,
        body: bytes[end + 4..].to_vec(),
    }
}

async fn respond(stream: &mut TcpStream, request: CapturedRequest, outage: bool) {
    if outage {
        write_http(
            stream,
            503,
            "application/json",
            br#"{"error":"scripted outage"}"#,
        )
        .await;
    } else if request.method == "GET"
        && (request.path.ends_with("/models") || request.path.contains("/models/"))
    {
        let model = request
            .path
            .rsplit('/')
            .next()
            .filter(|model| *model != "models")
            .unwrap_or(PRIVATE_MODEL);
        write_http(
            stream,
            200,
            "application/json",
            format!(r#"{{"id":"{model}","object":"model"}}"#).as_bytes(),
        )
        .await;
    } else if request.method == "POST" {
        let body: serde_json::Value =
            serde_json::from_slice(&request.body).expect("upstream Chat JSON");
        if body
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let usage = body
                .get("stream_options")
                .and_then(|value| value.get("include_usage"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let has_tools = body.get("tools").is_some();
            let first_delta = if has_tools {
                "{\"role\":\"assistant\"}"
            } else {
                "{\"role\":\"assistant\",\"content\":\"Scripted response.\"}"
            };
            let mut stream_body = format!(
                "data: {{\"id\":\"chatcmpl_scripted\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"{PRIVATE_MODEL}\",\"choices\":[{{\"index\":0,\"delta\":{first_delta},\"finish_reason\":null}}]}}\n\n"
            );
            if has_tools {
                stream_body.push_str("data: {\"id\":\"chatcmpl_scripted\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_scripted\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{}\"}}]},\"finish_reason\":null}]}\n\n");
            }
            stream_body.push_str(if has_tools { "data: {\"id\":\"chatcmpl_scripted\",\"model\":\"upstream-private-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n" } else { "data: {\"id\":\"chatcmpl_scripted\",\"model\":\"upstream-private-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n" });
            if usage {
                stream_body.push_str("data: {\"id\":\"chatcmpl_scripted\",\"model\":\"upstream-private-model\",\"created\":0,\"choices\":[],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1,\"total_tokens\":2}}\n\n");
            }
            stream_body.push_str("data: [DONE]\n\n");
            write_http(stream, 200, "text/event-stream", stream_body.as_bytes()).await;
        } else {
            let tool_calls = body.get("tools").is_some();
            let message = if tool_calls {
                r#"{"role":"assistant","content":null,"tool_calls":[{"id":"call_scripted","type":"function","function":{"name":"read_file","arguments":"{}"}}]}"#
                    .to_owned()
            } else {
                r#"{"role":"assistant","content":"Scripted response."}"#.to_owned()
            };
            let response = format!(
                r#"{{"id":"chatcmpl_scripted","object":"chat.completion","created":0,"model":"{PRIVATE_MODEL}","choices":[{{"index":0,"message":{message},"finish_reason":"{}"}}],"usage":{{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}}}"#,
                if tool_calls { "tool_calls" } else { "stop" }
            );
            write_http(stream, 200, "application/json", response.as_bytes()).await;
        }
    } else {
        write_http(stream, 404, "application/json", br#"{"error":"not found"}"#).await;
    }
}

async fn write_http(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let status_text = if status == 200 {
        "OK"
    } else {
        "Service Unavailable"
    };
    let response = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .expect("upstream response headers");
    stream
        .write_all(body)
        .await
        .expect("upstream response body");
}

fn parse_response(bytes: &[u8]) -> HttpResponse {
    let end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("response headers");
    let header = String::from_utf8_lossy(&bytes[..end]);
    let status = header
        .lines()
        .next()
        .expect("status line")
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("status number");
    HttpResponse {
        status,
        body: bytes[end + 4..].to_vec(),
    }
}

fn json(response: &HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("JSON response")
}
