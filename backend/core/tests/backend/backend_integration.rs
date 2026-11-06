//! Whole-backend integration behavior across catalog reads, model lifecycle,
//! application lifecycle, persistence, concurrency, and restart recovery.
//!
//! This suite pins composed backend services and isolated-fixture behavior, so
//! changes to owner boundaries, declaration/current-state handling, or
//! controller recovery fail before changing product behavior. It intentionally
//! excludes protocol wire/descriptor contracts, catalog source-only contracts,
//! transport compatibility, and hardware-gated runtime qualification.

use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use dotcore::{
    application::repository::CurrentApplicationRepository, composition::catalog_service_for,
};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationDeploymentDeclaration, ApplicationDeploymentDesiredState,
        ApplicationDeploymentState, ApplicationLifecycleState, ApplicationReadinessState,
        CreateApplicationDeploymentRequest, CurrentApplicationDeployment,
        GetApplicationDeploymentRequest, ListApplicationDeploymentsRequest, PauseDeploymentRequest,
        RemoveApplicationDeploymentRequest, ResumeDeploymentRequest,
        UpdateApplicationDeploymentMetadataRequest,
        application_deployment_service_server::ApplicationDeploymentService as ApplicationServiceContract,
    },
    catalog::v1::{
        EngineParameterValue, EngineParameterValues, ListApplicationsRequest,
        ListEngineReleasesRequest, ListModelsRequest, UseCase,
        catalog_service_server::CatalogService as CatalogServiceContract, engine_parameter_value,
    },
    deployment::v1::MemoryFit,
    inference::v1::{DeleteRouteRequest, InferenceRoute, ListRoutesRequest, ReplaceRouteRequest},
    model::v1::{
        CreateLocalModelDeploymentRequest, CreateModelDeploymentRequest, DirectRouteExposure,
        ListModelDeploymentsRequest, ListModelProfileCompatibilityRequest,
        ListModelRecommendationsRequest, ModelCompatibilityStatus, ModelDeployment,
        ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentExposure,
        ModelDeploymentMetadata, ModelDeploymentState, ModelDeploymentStatus,
        ModelRecommendationFit, PauseModelRequest, PreviewLocalModelDeploymentRequest,
        RecommendedModelProfile, RecommendedProfileTarget, RemoveModelDeploymentRequest,
        ResumeModelRequest, SizingScenario, SizingScenarioUnsupportedReason,
        UpdateModelDeploymentMetadataRequest, UseCaseModelRecommendations,
        create_model_deployment_request, model_deployment_exposure,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
    },
    site::store::v1::SiteCurrentState,
    site::v1::Site,
};
use dotcore_proto::google::protobuf::FieldMask;
use dotcore_runtime_backends::simulated::{
    SimulatedRuntimeCallCounts, SimulatedRuntimeGroupCall, SimulatedRuntimeGroupDrift,
    SimulatedRuntimeTestSupport,
};
use dotcore_site::store::FileCurrentState;
use prost::Message;
use support::{application_service_for, model_service_for};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tonic::Request;

use crate::support;

const FAST_TEST_SCENARIO: &str = support::FAST_RUNTIME_SCENARIO;

#[tokio::test]
async fn simulated_download_progress_precedes_runtime_loading_through_management_api() {
    const GIBIBYTE: u64 = 1024 * 1024 * 1024;

    let root = support::temporary_fast_fixture();
    let runtime = support::simulated_model_transition_test_support();
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.model_client();
    let deployment = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded deployment");

    tokio::time::timeout(
        support::DIAGNOSTIC_DEADLOCK_WATCHDOG,
        runtime.wait_for_preparation_gate(),
    )
    .await
    .expect("model preparation gate deadlock watchdog");
    let downloading = wait_for_lifecycle_client(
        &mut client,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Downloading,
    )
    .await
    .state
    .expect("downloading state");
    assert_eq!(
        downloading.status_message,
        "Simulating model artifact download"
    );
    let progress = downloading.progress.expect("download progress");
    assert_eq!(progress.completed, 0);
    assert_eq!(progress.total, Some(8 * GIBIBYTE));
    assert_eq!(
        progress.unit,
        dotcore_proto::dotcore::model::v1::ModelDeploymentProgressUnit::Bytes as i32
    );

    runtime.release_preparation_gate();
    tokio::time::timeout(
        support::DIAGNOSTIC_DEADLOCK_WATCHDOG,
        runtime.wait_for_readiness_gate(),
    )
    .await
    .expect("model readiness gate deadlock watchdog");
    let loading = wait_for_lifecycle_client(
        &mut client,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Loading,
    )
    .await
    .state
    .expect("loading state");
    assert_eq!(loading.status_message, "Loading");
    assert_eq!(loading.progress, None);

    runtime.release_readiness_gate();
    let ready = wait_for_lifecycle_client(
        &mut client,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await
    .state
    .expect("ready state");
    assert_eq!(ready.status, ModelDeploymentStatus::Ready as i32);
}

#[tokio::test]
async fn model_pause_resume_preserves_composed_records_and_serving_publication() {
    let root = support::temporary_fast_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    fs::write(
        &site_path,
        source.replace(
            FAST_TEST_SCENARIO,
            "defaultScenario:\n        readyDelayMillis: 100\n        stopDelayMillis: 100",
        ),
    )
    .expect("delayed pause fixture");
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let mut client = backend.model_client();
    let loaded = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let ready = deployment_from_client(&mut client, &loaded.model_deployment_id).await;
    assert_eq!(
        ready.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    assert_eq!(ready.snapshot, loaded.snapshot);
    assert_eq!(ready.intent, loaded.intent);
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id).status,
        ModelDeploymentStatus::Ready as i32
    );
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({"object": "list", "data": []})
    );

    let pausing = client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("pause model")
        .into_inner()
        .deployment
        .expect("pausing model deployment");
    assert_eq!(
        pausing.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(
        pausing.state.expect("pausing state").status,
        ModelDeploymentStatus::Pausing as i32
    );
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );
    assert_model_declaration(&root, &loaded, ModelDeploymentDesiredState::Paused);

    let paused = wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    assert_eq!(
        paused.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(paused.snapshot, loaded.snapshot);
    assert_eq!(paused.intent, loaded.intent);
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id).status,
        ModelDeploymentStatus::Paused as i32
    );

    let repeated_pause = client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("idempotent pause")
        .into_inner()
        .deployment
        .expect("idempotent paused deployment");
    assert_eq!(repeated_pause, paused);

    let loading = client
        .resume_model(ResumeModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("resume model")
        .into_inner()
        .deployment
        .expect("loading model deployment");
    assert_eq!(
        loading.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    assert_eq!(
        loading.state.expect("loading state").status,
        ModelDeploymentStatus::Loading as i32
    );
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );
    assert_model_declaration(&root, &loaded, ModelDeploymentDesiredState::Present);

    let ready_again = wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    assert_eq!(
        ready_again.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    assert_eq!(ready_again.snapshot, loaded.snapshot);
    assert_eq!(ready_again.intent, loaded.intent);
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id).status,
        ModelDeploymentStatus::Ready as i32
    );
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({"object": "list", "data": []})
    );

    let repeated_resume = client
        .resume_model(ResumeModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("idempotent resume")
        .into_inner()
        .deployment
        .expect("idempotent ready deployment");
    assert_eq!(repeated_resume, ready_again);

    client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload model");
    wait_for_deployment_removal_client(&mut client).await;
    backend.stop().await;
}

#[tokio::test]
async fn simulated_model_withdrawal_preserves_unrelated_model_discovery_and_inference() {
    let root = support::temporary_fast_fixture();
    let second_model_path = root.join("catalog/models/fixture-model-b.yaml");
    let second_model = fs::read_to_string(root.join("catalog/models/fixture-model.yaml"))
        .expect("Simulated model fixture");
    fs::write(
        &second_model_path,
        second_model
            .replace("modelId: \"fixture-model\"", "modelId: \"fixture-model-b\"")
            .replace(
                "displayName: \"Fixture Model\"",
                "displayName: \"Fixture Model B\"",
            ),
    )
    .expect("second simulated model fixture");

    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(1);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.model_client();
    let model_a = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: "fixture-model".to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("load model A")
        .into_inner()
        .deployment
        .expect("Model A deployment");
    let model_b = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model-b".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: "fixture-model-b".to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("load model B")
        .into_inner()
        .deployment
        .expect("Model B deployment");
    wait_for_lifecycle_client(
        &mut client,
        &model_a.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    wait_for_lifecycle_client(
        &mut client,
        &model_b.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": [
                {"id": "fixture-model", "object": "model", "created": 0, "owned_by": "dotcore"},
                {"id": "fixture-model-b", "object": "model", "created": 0, "owned_by": "dotcore"}
            ]
        })
    );
    assert_eq!(
        served_chat(backend.inference_address(), "fixture-model-b").await,
        serde_json::json!({
            "id": "chatcmpl_simulated",
            "object": "chat.completion",
            "created": 0,
            "model": "fixture-model-b",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Simulated response."},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 3, "total_tokens": 4}
        })
    );

    let withdrawing = client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: model_a.model_deployment_id.clone(),
        })
        .await
        .expect("withdraw model A")
        .into_inner()
        .deployment
        .expect("withdrawing model A deployment");
    assert_eq!(
        withdrawing.state.expect("withdrawing model A state").status,
        ModelDeploymentStatus::Unloading as i32
    );
    wait_for_simulated_stop_failure(&runtime).await;
    assert_eq!(runtime.call_counts().failed_stops, 1);

    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": [{"id": "fixture-model-b", "object": "model", "created": 0, "owned_by": "dotcore"}]
        })
    );
    assert_eq!(
        served_chat(backend.inference_address(), "fixture-model-b").await,
        serde_json::json!({
            "id": "chatcmpl_simulated",
            "object": "chat.completion",
            "created": 0,
            "model": "fixture-model-b",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "Simulated response."},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 3, "total_tokens": 4}
        })
    );

    client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: model_b.model_deployment_id,
        })
        .await
        .expect("unload model B");
    wait_for_deployment_removal_client(&mut client).await;
    backend.stop().await;
}

#[tokio::test]
async fn application_retains_its_creation_base_url_across_inference_listener_changes() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let first_runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let reserved_inference_port = dotcore_domain::test_support::ports::reserve_port();
    let backend = support::RunningBackend::start_on_inference_port(
        configuration.clone(),
        first_runtime.clone(),
        reserved_inference_port.port(),
    )
    .await;
    let first_port = backend.inference_address().port();
    let mut client = backend.application_client();
    let created = create_application(&mut client).await;
    let first_ready = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    let first_launches = first_runtime.group_launches();
    assert_eq!(first_launches.len(), 1);
    let first_launch = &first_launches[0];
    let site_path = root.join("site.yaml");
    let desired_before = fs::read(&site_path).expect("retained desired site");
    let site: Site = dotcore_proto::yaml::from_file(&site_path).expect("Site node");
    let host = &site.nodes[0].host;
    let creation_base_url = format!("http://{host}:{first_port}/v1");
    assert_eq!(site.nodes[0].node_id, "fixture-node");
    assert_eq!(first_launch.group_id.0, created.application_deployment_id);
    assert_eq!(
        first_launch.units[&dotcore_runtime::WorkloadUnitName("web".to_owned())].environment["OPENAI_API_BASE_URL"],
        creation_base_url,
    );

    backend.stop().await;
    support::wait_for_site_store_release(&support::configuration(&root)).await;
    let old_port_guard = std::net::TcpListener::bind(("127.0.0.1", first_port))
        .expect("reserve released old inference port");
    let second_runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let restarted = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration,
        second_runtime.clone(),
    )
    .await;
    assert_eq!(first_port, reserved_inference_port.port());
    assert_ne!(restarted.inference_address().port(), first_port);
    let mut restarted_client = restarted.application_client();
    wait_for_group_launches(&second_runtime, 1).await;
    let second_ready = wait_for_fresh_application_lifecycle(
        &mut restarted_client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    let second_launches = second_runtime.group_launches();
    assert_eq!(second_launches.len(), 1);
    let second_launch = &second_launches[0];
    assert_eq!(
        fs::read(&site_path).expect("unchanged desired site"),
        desired_before
    );
    assert_eq!(
        second_ready.application_deployment_id,
        first_ready.application_deployment_id
    );
    assert_eq!(second_ready.snapshot, first_ready.snapshot);
    assert_eq!(second_launch.group_id, first_launch.group_id);
    assert_eq!(
        second_launch.units[&dotcore_runtime::WorkloadUnitName("web".to_owned())].environment["OPENAI_API_BASE_URL"],
        creation_base_url,
    );
    restarted.stop().await;
    drop(old_port_guard);
    drop(reserved_inference_port);
}

#[tokio::test]
async fn application_pause_resume_preserves_composed_records_endpoint_and_runtime_identity() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration.clone(),
        runtime.clone(),
    )
    .await;
    let mut client = backend.application_client();
    let created = create_application(&mut client).await;
    let ready = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_application_ready(&ready, health.port());
    let runtime_calls_before_rename = runtime.group_calls();
    let renamed = client
        .update_application_deployment_metadata(UpdateApplicationDeploymentMetadataRequest {
            application_deployment_id: created.application_deployment_id.clone(),
            metadata: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                    display_name: Some("Application metadata rename".to_owned()),
                },
            ),
            update_mask: Some(dotcore_proto::google::protobuf::FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        })
        .await
        .expect("rename ready application")
        .into_inner()
        .deployment
        .expect("renamed application");
    assert_eq!(
        renamed.application_deployment_id,
        created.application_deployment_id
    );
    assert_eq!(
        renamed.application_deployment_name,
        "Application metadata rename"
    );
    assert_eq!(renamed.snapshot, ready.snapshot);
    assert_eq!(
        renamed.state.as_ref().map(|state| state.lifecycle_state),
        ready.state.as_ref().map(|state| state.lifecycle_state)
    );
    assert_eq!(
        renamed.state.as_ref().map(|state| state.readiness_state),
        ready.state.as_ref().map(|state| state.readiness_state)
    );
    assert_eq!(
        renamed
            .state
            .as_ref()
            .and_then(|state| state.endpoint.clone()),
        ready
            .state
            .as_ref()
            .and_then(|state| state.endpoint.clone())
    );
    assert_eq!(runtime.group_calls(), runtime_calls_before_rename);

    let declaration = application_declaration(&root, &created.application_deployment_id);
    let current = application_current(&root, &created.application_deployment_id);
    assert_eq!(
        declaration.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    assert_eq!(declaration.placement, created.placement);
    assert_eq!(declaration.snapshot, created.snapshot);
    assert_eq!(declaration.storage_attachments, created.storage_attachments);
    let current_state = current.state.expect("current application state");
    assert_eq!(
        current_state.lifecycle_state,
        ApplicationLifecycleState::Ready as i32
    );
    assert_eq!(
        current_state.readiness_state,
        ApplicationReadinessState::Ready as i32
    );
    assert_eq!(
        current_state.endpoint,
        renamed.state.and_then(|state| state.endpoint)
    );
    assert!(runtime.storage_exists(&deployment_data(&created.application_deployment_id)));

    let pausing = client
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("pause application")
        .into_inner()
        .deployment
        .expect("pausing application");
    assert_application_suppressed(&pausing, ApplicationLifecycleState::Pausing);
    assert_eq!(
        application_declaration(&root, &created.application_deployment_id).desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    let paused = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    assert_application_suppressed(&paused, ApplicationLifecycleState::Paused);
    assert_eq!(
        application_current(&root, &created.application_deployment_id).state,
        paused.state
    );
    assert_eq!(paused.snapshot, created.snapshot);
    assert_eq!(paused.placement, created.placement);
    assert_eq!(paused.storage_attachments, created.storage_attachments);
    assert!(runtime.storage_exists(&deployment_data(&created.application_deployment_id)));
    // The node handles the rename before the pause, so the stop following the create directly
    // proves the rename relaunched nothing.
    assert_eq!(
        runtime.group_calls(),
        vec![
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
        ]
    );
    let repeated_pause = client
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("idempotent pause")
        .into_inner()
        .deployment
        .expect("repeated paused application");
    // Mutation responses carry no read-time accelerator memory projection.
    assert_eq!(
        repeated_pause,
        ApplicationDeployment {
            service_accelerator_memory: Vec::new(),
            ..paused.clone()
        }
    );

    backend.stop().await;
    support::wait_for_site_store_release(&support::configuration(&root)).await;
    let restarted = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration,
        runtime.clone(),
    )
    .await;
    let mut restarted_client = restarted.application_client();
    // The restarted node reports the paused deployment whose group is gone.
    let recovered = wait_for_application_lifecycle(
        &mut restarted_client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Unavailable,
    )
    .await;
    assert_eq!(
        recovered.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    assert_application_suppressed(&recovered, ApplicationLifecycleState::Unavailable);
    assert_eq!(recovered.snapshot, created.snapshot);
    assert_eq!(recovered.placement, created.placement);
    assert_eq!(recovered.storage_attachments, created.storage_attachments);
    assert!(runtime.storage_exists(&deployment_data(&created.application_deployment_id)));

    let starting = restarted_client
        .resume_deployment(ResumeDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("resume application")
        .into_inner()
        .deployment
        .expect("starting application");
    assert_application_starting(&starting);
    let ready_again = wait_for_fresh_application_lifecycle(
        &mut restarted_client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_application_ready(&ready_again, health.port());
    assert_eq!(ready_again.snapshot, created.snapshot);
    assert_eq!(ready_again.placement, created.placement);
    assert_eq!(ready_again.storage_attachments, created.storage_attachments);
    assert!(runtime.storage_exists(&deployment_data(&created.application_deployment_id)));
    let repeated_resume = restarted_client
        .resume_deployment(ResumeDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("idempotent resume")
        .into_inner()
        .deployment
        .expect("repeated ready application");
    assert_eq!(
        repeated_resume,
        ApplicationDeployment {
            service_accelerator_memory: Vec::new(),
            ..ready_again.clone()
        }
    );
    assert_eq!(
        runtime.group_calls(),
        vec![
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
        ]
    );
    restarted.stop().await;
}

#[tokio::test]
async fn application_group_stop_and_start_failures_retry_without_losing_pause_or_endpoint_safety() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port())
        .with_group_stop_failures(1)
        .with_group_start_failures(1);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.application_client();
    let created = create_application(&mut client).await;
    wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;

    let pausing = client
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("pause acceptance")
        .into_inner()
        .deployment
        .expect("pausing application");
    assert_application_suppressed(&pausing, ApplicationLifecycleState::Pausing);
    wait_for_group_calls(&runtime, 2).await;
    wake_node_reconciliation(
        &mut client,
        &created.application_deployment_id,
        "retry stop",
    )
    .await;
    wait_for_group_calls(&runtime, 3).await;
    let paused = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    assert_application_suppressed(&paused, ApplicationLifecycleState::Paused);
    assert_eq!(
        runtime.group_calls(),
        vec![
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
        ]
    );

    let starting = client
        .resume_deployment(ResumeDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("resume acceptance")
        .into_inner()
        .deployment
        .expect("starting application");
    assert_application_starting(&starting);
    wait_for_group_calls(&runtime, 4).await;
    wake_node_reconciliation(
        &mut client,
        &created.application_deployment_id,
        "retry start",
    )
    .await;
    wait_for_group_calls(&runtime, 5).await;
    let ready = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_application_ready(&ready, health.port());
    assert_eq!(
        runtime.group_calls(),
        vec![
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::start(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::start(&created.application_deployment_id),
        ]
    );
    backend.stop().await;
}

#[tokio::test]
async fn application_paused_absent_partial_and_stale_drift_never_projects_endpoint() {
    for drift in [
        SimulatedRuntimeGroupDrift::Absent,
        SimulatedRuntimeGroupDrift::Partial,
        SimulatedRuntimeGroupDrift::Stale,
    ] {
        let health = application_health_server().await;
        let root = support::temporary_fast_fixture();
        let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
        let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
            support::configuration(&root),
            runtime.clone(),
        )
        .await;
        let mut client = backend.application_client();
        let created = create_application(&mut client).await;
        let ready = wait_for_application_lifecycle(
            &mut client,
            &created.application_deployment_id,
            ApplicationLifecycleState::Ready,
        )
        .await;
        assert_application_ready(&ready, health.port());
        let pausing = client
            .pause_deployment(PauseDeploymentRequest {
                application_deployment_id: created.application_deployment_id.clone(),
            })
            .await
            .expect("pause acceptance")
            .into_inner()
            .deployment
            .expect("pausing application");
        assert_application_suppressed(&pausing, ApplicationLifecycleState::Pausing);
        wait_for_application_lifecycle(
            &mut client,
            &created.application_deployment_id,
            ApplicationLifecycleState::Paused,
        )
        .await;
        // Drift is injected only once Paused is observed: every later reconcile reads the paused
        // intent, so no running-intent pass can repair the drifted group. The rename only wakes
        // reconciliation so the drift is observed without waiting for the cadence.
        runtime.set_group_drift(&created.application_deployment_id, Some(drift));
        wake_node_reconciliation(&mut client, &created.application_deployment_id, "drifted").await;
        let unavailable = wait_for_application_lifecycle(
            &mut client,
            &created.application_deployment_id,
            ApplicationLifecycleState::Unavailable,
        )
        .await;
        assert_application_suppressed(&unavailable, ApplicationLifecycleState::Unavailable);
        assert_eq!(unavailable.snapshot, created.snapshot);
        assert_eq!(unavailable.placement, created.placement);
        assert_eq!(unavailable.storage_attachments, created.storage_attachments);
        runtime.set_group_drift(&created.application_deployment_id, None);
        client
            .resume_deployment(ResumeDeploymentRequest {
                application_deployment_id: created.application_deployment_id.clone(),
            })
            .await
            .expect("resume drifted application");
        let resumed = wait_for_application_lifecycle(
            &mut client,
            &created.application_deployment_id,
            ApplicationLifecycleState::Ready,
        )
        .await;
        assert_application_ready(&resumed, health.port());
        assert_eq!(
            runtime.group_calls(),
            vec![
                SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
                SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
                SimulatedRuntimeGroupCall::start(&created.application_deployment_id),
            ],
            "{drift:?} drift while paused must not create, remove, or stop the group again"
        );
        backend.stop().await;
    }
}

#[tokio::test]
async fn application_remove_from_paused_is_idempotent_serialized_and_deletes_data() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut loader = backend.application_client();
    let created = create_application(&mut loader).await;
    wait_for_application_lifecycle(
        &mut loader,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    loader
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("pause application");
    let paused = wait_for_application_lifecycle(
        &mut loader,
        &created.application_deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    let declaration = application_declaration(&root, &created.application_deployment_id);
    assert!(
        runtime.storage_exists(&deployment_data(&created.application_deployment_id)),
        "pause keeps the deployment's data"
    );
    let calls_before = runtime.group_calls();
    assert_application_suppressed(&paused, ApplicationLifecycleState::Paused);

    let mut pauser = backend.application_client();
    let mut resumer = backend.application_client();
    let mut remover = backend.application_client();
    let id = created.application_deployment_id.clone();
    let (pause, resume, remove) = tokio::join!(
        pauser.pause_deployment(PauseDeploymentRequest {
            application_deployment_id: id.clone()
        }),
        resumer.resume_deployment(ResumeDeploymentRequest {
            application_deployment_id: id.clone()
        }),
        remover.remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: id.clone()
        }),
    );
    assert!(
        pause.is_ok()
            || matches!(
                pause.as_ref().err().map(tonic::Status::code),
                Some(tonic::Code::FailedPrecondition | tonic::Code::NotFound)
            )
    );
    assert!(
        resume.is_ok()
            || matches!(
                resume.as_ref().err().map(tonic::Status::code),
                Some(tonic::Code::FailedPrecondition | tonic::Code::NotFound)
            )
    );
    assert!(
        remove.is_ok(),
        "Remove from paused must be accepted: {remove:?}"
    );
    wait_for_application_removal(&mut loader).await;
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("final site");
    let state = SiteCurrentState::decode(
        fs::read(root.join("state.pb"))
            .expect("final state")
            .as_slice(),
    )
    .expect("final state protobuf");
    assert!(site.application_deployments.is_empty());
    assert!(state.application_deployments.is_empty());
    assert!(
        !runtime.storage_exists(&deployment_data(&created.application_deployment_id)),
        "removal deletes the deployment's data"
    );
    assert_eq!(application_declaration_count(&root), 0);
    let calls = runtime.group_calls();
    assert!(
        calls[calls_before.len()..]
            .iter()
            .all(|call| call.operation != "start")
    );
    assert!(
        declaration
            .storage_attachments
            .iter()
            .any(|attachment| attachment.storage_name == "web-data")
    );
    backend.stop().await;
}

#[tokio::test]
async fn application_paused_snapshot_survives_catalog_removal_and_remove_never_starts_work() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    fs::create_dir_all(root.join("catalog/applications")).expect("Application catalog directory");
    fs::write(
        root.join("catalog/applications/open-webui.yaml"),
        support::with_oci_service_images(
            &fs::read_to_string(
                support::workspace_root().join("catalog/applications/open-webui.yaml"),
            )
            .expect("local application catalog release"),
        ),
    )
    .expect("local application catalog release");
    let empty_master = root.join("empty-master-catalog");
    for directory in ["models", "engines", "applications"] {
        fs::create_dir_all(empty_master.join(directory)).expect("empty catalog directory");
    }
    let configuration = dotcore_domain::SitePaths::from_values(
        Some(root.to_str().expect("Site root")),
        Some(empty_master.to_str().expect("empty catalog root")),
        Some(root.join("cache").to_str().expect("artifact cache")),
    )
    .expect("local catalog configuration");
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration.clone(),
        runtime.clone(),
    )
    .await;
    let mut client = backend.application_client();
    let created = create_application(&mut client).await;
    let ready = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_application_ready(&ready, health.port());
    client
        .pause_deployment(PauseDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("pause application");
    let paused = wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Paused,
    )
    .await;
    let snapshot = paused.snapshot.clone();
    let attachments = paused.storage_attachments.clone();
    backend.stop().await;
    support::wait_for_site_store_release(&support::configuration(&root)).await;
    fs::remove_file(root.join("catalog/applications/open-webui.yaml"))
        .expect("remove catalog application");

    let restarted = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration,
        runtime.clone(),
    )
    .await;
    let mut restarted_client = restarted.application_client();
    // The restarted node reports the paused deployment whose group is gone.
    let recovered = wait_for_application_lifecycle(
        &mut restarted_client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Unavailable,
    )
    .await;
    assert_eq!(
        recovered.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    assert_application_suppressed(&recovered, ApplicationLifecycleState::Unavailable);
    assert_eq!(recovered.snapshot, snapshot);
    assert_eq!(recovered.storage_attachments, attachments);
    assert!(runtime.storage_exists(&deployment_data(&created.application_deployment_id)));
    let removing = restarted_client
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("Remove paused application without catalog")
        .into_inner()
        .deployment
        .expect("teardown application");
    assert_eq!(
        removing.desired_state,
        ApplicationDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(
        removing
            .state
            .as_ref()
            .expect("teardown state")
            .lifecycle_state,
        ApplicationLifecycleState::Stopping as i32
    );
    wait_for_application_removal(&mut restarted_client).await;
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("final site");
    let state = SiteCurrentState::decode(
        fs::read(root.join("state.pb"))
            .expect("final state")
            .as_slice(),
    )
    .expect("final state protobuf");
    assert!(site.application_deployments.is_empty());
    assert!(state.application_deployments.is_empty());
    assert!(!runtime.storage_exists(&deployment_data(&created.application_deployment_id)));
    assert_eq!(
        runtime.group_calls(),
        vec![
            SimulatedRuntimeGroupCall::create(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::stop(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::ensure_absent(&created.application_deployment_id),
            SimulatedRuntimeGroupCall::remove_storage(&created.application_deployment_id),
        ]
    );
    restarted.stop().await;
}

#[tokio::test]
async fn paused_model_restart_retains_exact_records_and_recovers_after_catalog_removal() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let backend = support::RunningBackend::start(configuration.clone()).await;
    let mut client = backend.model_client();
    let loaded = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("pause model");
    let paused = wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    let paused_site = model_declaration(&root, &loaded.model_deployment_id);
    let paused_state = state_record(&root, &loaded.model_deployment_id);
    assert_eq!(
        paused.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(paused.snapshot, loaded.snapshot);
    assert_eq!(paused.intent, loaded.intent);
    assert_eq!(paused_state.status, ModelDeploymentStatus::Paused as i32);
    backend.stop().await;
    support::wait_for_site_store_release(&support::configuration(&root)).await;

    let prepared = support::prepare_backend(configuration.clone(), async {})
        .await
        .expect("restart preparation");
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id),
        paused_state
    );
    drop(prepared);
    fs::remove_file(root.join("catalog/models/fixture-model.yaml")).expect("remove catalog model");

    let restarted = support::RunningBackend::start(configuration).await;
    let mut restarted_client = restarted.model_client();
    let unavailable = wait_for_lifecycle_client(
        &mut restarted_client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Unavailable,
    )
    .await;
    assert_eq!(
        unavailable.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(unavailable.snapshot, loaded.snapshot);
    assert_eq!(unavailable.intent, loaded.intent);
    assert_eq!(
        model_declaration(&root, &loaded.model_deployment_id),
        paused_site
    );
    assert_eq!(
        served_models(restarted.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );

    let resumed = restarted_client
        .resume_model(ResumeModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("resume from immutable snapshot")
        .into_inner()
        .deployment
        .expect("resumed model deployment");
    assert_eq!(
        resumed.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    let ready = wait_for_lifecycle_client(
        &mut restarted_client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    assert_eq!(ready.snapshot, loaded.snapshot);
    assert_eq!(ready.intent, loaded.intent);
    assert_eq!(
        served_models(restarted.inference_address()).await,
        serde_json::json!({"object": "list", "data": []})
    );
    restarted_client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload recovered model");
    wait_for_deployment_removal_client(&mut restarted_client).await;
    restarted.stop().await;
}

#[tokio::test]
async fn paused_model_stop_retry_converges_from_pausing_to_paused() {
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(0).with_stop_gate();
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.model_client();
    let loaded = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    let pausing = client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("pause model")
        .into_inner()
        .deployment
        .expect("pausing model deployment");
    assert_eq!(
        pausing.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(
        pausing.state.expect("pausing state").status,
        ModelDeploymentStatus::Pausing as i32
    );
    // The held stop keeps the workload stopping, so the deployment stays Pausing.
    tokio::time::timeout(
        support::DIAGNOSTIC_DEADLOCK_WATCHDOG,
        runtime.wait_for_stop_gate(),
    )
    .await
    .expect("workload stop gate deadlock watchdog");
    assert_eq!(
        deployment_from_client(&mut client, &loaded.model_deployment_id)
            .await
            .state
            .expect("held pausing state")
            .status,
        ModelDeploymentStatus::Pausing as i32
    );
    runtime.release_stop_gate();
    let paused = wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    assert_eq!(
        paused.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id).status,
        ModelDeploymentStatus::Paused as i32
    );
    client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload paused model");
    wait_for_deployment_removal_client(&mut client).await;
    backend.stop().await;
}

#[tokio::test]
async fn simulated_stop_failure_keeps_pause_durable_and_retries_without_recreation() {
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(1);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.model_client();
    let loaded = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let pausing = client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("pause model")
        .into_inner()
        .deployment
        .expect("pausing model deployment");
    assert_eq!(
        pausing.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(
        pausing.state.expect("pausing state").status,
        ModelDeploymentStatus::Pausing as i32
    );
    assert_model_declaration(&root, &loaded, ModelDeploymentDesiredState::Paused);
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );

    wait_for_simulated_stop_failure(&runtime).await;

    let paused = wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    assert_eq!(
        paused.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(
        state_record(&root, &loaded.model_deployment_id).status,
        ModelDeploymentStatus::Paused as i32
    );
    assert_eq!(
        runtime.call_counts(),
        SimulatedRuntimeCallCounts {
            creates: 1,
            starts: 0,
            stops: 2,
            failed_stops: 1,
        }
    );

    client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload paused model");
    wait_for_deployment_removal_client(&mut client).await;
    backend.stop().await;
}

#[tokio::test]
async fn paused_model_stale_current_state_fails_closed_until_resume() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let backend = support::RunningBackend::start(configuration.clone()).await;
    let mut client = backend.model_client();
    let loaded = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    client
        .pause_model(PauseModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("pause model");
    wait_for_lifecycle_client(
        &mut client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    backend.stop().await;
    support::wait_for_site_store_release(&support::configuration(&root)).await;

    let state_path = root.join("state.pb");
    let mut state =
        SiteCurrentState::decode(fs::read(&state_path).expect("current state").as_slice())
            .expect("current state protobuf");
    let record = state
        .model_deployments
        .iter_mut()
        .find(|deployment| deployment.model_deployment_id == loaded.model_deployment_id)
        .expect("stale model current record");
    let current = record.state.as_mut().expect("stale model state");
    current.status = ModelDeploymentStatus::Ready as i32;
    current.status_message = "ready".to_owned();
    fs::write(&state_path, state.encode_to_vec()).expect("stale current state");

    // The restarted node reports nothing for the paused deployment, so the stale Ready record
    // stays fenced as Unavailable until resume; the fencing reaches the retained record after the
    // public projection shows it.
    let restarted = support::RunningBackend::start(configuration).await;
    let mut restarted_client = restarted.model_client();
    let unavailable = wait_for_lifecycle_client(
        &mut restarted_client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Unavailable,
    )
    .await;
    assert_eq!(
        unavailable.desired_state,
        ModelDeploymentDesiredState::Paused as i32
    );
    assert_eq!(unavailable.snapshot, loaded.snapshot);
    assert_eq!(unavailable.intent, loaded.intent);
    wait_for_state_record_status(
        &root,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Unavailable,
    )
    .await;
    assert_eq!(
        served_models(restarted.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );

    restarted_client
        .resume_model(ResumeModelRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        })
        .await
        .expect("resume stale model");
    let ready = wait_for_lifecycle_client(
        &mut restarted_client,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    assert_eq!(ready.snapshot, loaded.snapshot);
    assert_eq!(ready.intent, loaded.intent);
    restarted_client
        .remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        })
        .await
        .expect("unload resumed model");
    wait_for_deployment_removal_client(&mut restarted_client).await;
    restarted.stop().await;
}

#[tokio::test]
async fn concurrent_pause_resume_and_unload_leave_teardown_as_the_only_durable_outcome() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;
    let mut loader = backend.model_client();
    let loaded = loader
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("load model")
        .into_inner()
        .deployment
        .expect("loaded model deployment");
    wait_for_lifecycle_client(
        &mut loader,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let mut pauser = backend.model_client();
    let mut resumer = backend.model_client();
    let mut unloader = backend.model_client();
    let deployment_id = loaded.model_deployment_id.clone();
    let (pause, resume, unload) = tokio::join!(
        pauser.pause_model(PauseModelRequest {
            model_deployment_id: deployment_id.clone(),
        }),
        resumer.resume_model(ResumeModelRequest {
            model_deployment_id: deployment_id.clone(),
        }),
        unloader.remove_model_deployment(RemoveModelDeploymentRequest {
            model_deployment_id: deployment_id
        }),
    );
    assert!(
        pause.is_ok()
            || matches!(
                pause.as_ref().err().map(tonic::Status::code),
                Some(tonic::Code::FailedPrecondition | tonic::Code::NotFound)
            )
    );
    assert!(
        resume.is_ok()
            || matches!(
                resume.as_ref().err().map(tonic::Status::code),
                Some(tonic::Code::FailedPrecondition | tonic::Code::NotFound)
            )
    );
    assert!(unload.is_ok(), "teardown must be accepted: {unload:?}");
    wait_for_deployment_removal_client(&mut loader).await;
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("final site");
    let state = SiteCurrentState::decode(
        fs::read(root.join("state.pb"))
            .expect("final state")
            .as_slice(),
    )
    .expect("final state protobuf");
    assert!(site.model_deployments.is_empty());
    assert!(state.model_deployments.is_empty());
    assert_eq!(
        served_models(backend.inference_address()).await,
        serde_json::json!({
            "object": "list",
            "data": []
        })
    );
    backend.stop().await;
}

#[tokio::test]
async fn application_route_link_projects_its_name_without_availability_gating() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    configure_route_name_application(&root);
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let route = create_inference_route(&backend).await;
    let model_deployment_id = route.fallback_groups[0].model_deployment_ids[0].clone();
    let mut applications = backend.application_client();
    let created = create_route_test_application(&mut applications, &route).await;
    assert_eq!(
        created
            .selected_inference_route
            .as_ref()
            .map(|route| (route.inference_route_id.as_str(), route.route_name.as_str(),)),
        Some((route.inference_route_id.as_str(), "fixture-route"))
    );
    assert_eq!(
        application_declaration(&root, &created.application_deployment_id)
            .inference_route_id
            .as_deref(),
        Some(route.inference_route_id.as_str())
    );
    let deployment_id = created.application_deployment_id.clone();
    wait_for_application_lifecycle(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_eq!(
        launched_route_name(&runtime, &deployment_id),
        Some("fixture-route".to_owned())
    );
    let calls_before_changes = group_calls_for(&runtime, &deployment_id);

    let mut routes = backend.route_client();
    let policy_only = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: route.inference_route_id.clone(),
            expected_generation: route.generation,
            name: route.name.clone(),
            enabled: route.enabled,
            fallback_groups: route.fallback_groups.clone(),
        })
        .await
        .expect("policy-only route generation")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("replaced route");

    let mut models = backend.model_client();
    models
        .pause_model(PauseModelRequest {
            model_deployment_id: model_deployment_id.clone(),
        })
        .await
        .expect("pause route member");
    wait_for_lifecycle_client(
        &mut models,
        &model_deployment_id,
        ModelDeploymentStatus::Paused,
    )
    .await;
    let unserved = create_route_test_application(&mut applications, &policy_only).await;
    wait_for_application_lifecycle(
        &mut applications,
        &unserved.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_eq!(
        launched_route_name(&runtime, &unserved.application_deployment_id),
        Some("fixture-route".to_owned())
    );

    let disabled = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: policy_only.inference_route_id.clone(),
            expected_generation: policy_only.generation,
            name: policy_only.name.clone(),
            enabled: false,
            fallback_groups: policy_only.fallback_groups.clone(),
        })
        .await
        .expect("disable route")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("disabled route");
    let disabled_link = create_route_test_application(&mut applications, &disabled).await;
    wait_for_application_lifecycle(
        &mut applications,
        &disabled_link.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_eq!(
        launched_route_name(&runtime, &disabled_link.application_deployment_id),
        Some("fixture-route".to_owned())
    );

    let enabled = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: disabled.inference_route_id.clone(),
            expected_generation: disabled.generation,
            name: disabled.name.clone(),
            enabled: true,
            fallback_groups: disabled.fallback_groups.clone(),
        })
        .await
        .expect("enable route")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("enabled route");
    let unchanged = wait_for_application_lifecycle(
        &mut applications,
        &deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    assert_eq!(
        unchanged.selected_inference_route,
        created.selected_inference_route
    );
    assert_eq!(
        group_calls_for(&runtime, &deployment_id),
        calls_before_changes,
        "member, policy, disable, and enable changes must not relaunch the application"
    );

    let renamed = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: enabled.inference_route_id.clone(),
            expected_generation: enabled.generation,
            name: "fixture-route-renamed".to_owned(),
            enabled: true,
            fallback_groups: enabled.fallback_groups.clone(),
        })
        .await
        .expect("rename route")
        .into_inner()
        .route
        .and_then(|view| view.route)
        .expect("renamed route");
    let renamed_application =
        wait_for_application_route_name(&mut applications, &deployment_id, "fixture-route-renamed")
            .await;
    assert_eq!(renamed.inference_route_id, route.inference_route_id);
    assert_eq!(
        renamed_application
            .selected_inference_route
            .as_ref()
            .map(|selection| selection.route_name.as_str()),
        Some("fixture-route-renamed")
    );
    let rename_calls =
        wait_for_deployment_group_calls(&runtime, &deployment_id, calls_before_changes.len() + 2)
            .await;
    // The node handles changes in order, so the rename's relaunch being the next calls proves the
    // earlier member, policy, disable and enable changes relaunched nothing.
    assert_eq!(
        rename_calls[calls_before_changes.len()..],
        [
            SimulatedRuntimeGroupCall::remove(&deployment_id),
            SimulatedRuntimeGroupCall::create(&deployment_id),
        ],
        "member, policy, disable and enable changes must not relaunch the application"
    );
    assert_eq!(
        launched_route_name(&runtime, &deployment_id),
        Some("fixture-route-renamed".to_owned())
    );
    backend.stop().await;
}

#[tokio::test]
async fn route_input_application_without_a_link_reaches_ready_with_its_literal_fallback() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    configure_route_name_application(&root);
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut applications = backend.application_client();
    let created = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "route-test".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("create application without a route link")
        .into_inner()
        .deployment
        .expect("created application");
    assert_eq!(created.selected_inference_route, None);
    wait_for_application_lifecycle(
        &mut applications,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;

    let site: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("retained desired site");
    let launches = runtime.group_launches();
    assert_eq!(launches.len(), 1);
    let environment =
        &launches[0].units[&dotcore_runtime::WorkloadUnitName("web".to_owned())].environment;
    assert_eq!(environment["ROUTE_NAME"], "");
    assert_eq!(
        environment["OPENAI_API_BASE_URL"],
        format!(
            "http://{}:{}/v1",
            site.nodes[0].host,
            backend.inference_address().port()
        )
    );
    backend.stop().await;
}

#[tokio::test]
async fn referenced_application_route_cannot_be_deleted_until_application_removed() {
    let health = application_health_server().await;
    let root = support::temporary_fast_fixture();
    configure_route_name_application(&root);
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(health.port());
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime,
    )
    .await;
    let route = create_inference_route(&backend).await;
    let mut applications = backend.application_client();
    let created = applications
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "route-test".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: Some(route.inference_route_id.clone()),
            expected_inference_route_generation: route.generation,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("create route-bound application")
        .into_inner()
        .deployment
        .expect("created application");
    wait_for_application_lifecycle(
        &mut applications,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;

    let before_rejection = fs::read(root.join("site.yaml")).expect("Site before route deletion");
    let mut routes = backend.route_client();
    let status = routes
        .delete_route(DeleteRouteRequest {
            inference_route_id: route.inference_route_id.clone(),
            expected_generation: route.generation,
        })
        .await
        .expect_err("referenced route deletion");
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert!(
        status
            .message()
            .contains(&created.application_deployment_id)
    );
    assert_eq!(
        fs::read(root.join("site.yaml")).expect("Site after rejected route deletion"),
        before_rejection
    );

    applications
        .remove_application_deployment(RemoveApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        })
        .await
        .expect("remove route-bound application");
    wait_for_application_removal(&mut applications).await;
    routes
        .delete_route(DeleteRouteRequest {
            inference_route_id: route.inference_route_id,
            expected_generation: route.generation,
        })
        .await
        .expect("delete route after application removal");
    backend.stop().await;
}

type ModelClient = dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient<
    tonic::transport::Channel,
>;

type ApplicationClient =
    dotcore_proto::dotcore::application::v1::application_deployment_service_client::ApplicationDeploymentServiceClient<
        tonic::transport::Channel,
    >;

fn configure_route_name_application(root: &Path) {
    let source =
        fs::read_to_string(support::workspace_root().join("catalog/applications/open-webui.yaml"))
            .expect("route-name application source");
    let source = support::with_oci_service_images(&source);
    let source = source
        .replace("applicationId: \"open-webui\"", "applicationId: \"route-test\"")
        .replace("displayName: \"Open WebUI\"", "displayName: \"Route Test\"")
        .replace(
            "      storageBindings:",
            "        - bindingName: \"route-name\"\n          environmentVariable: \"ROUTE_NAME\"\n          literalValue: \"\"\n          input:\n            inferenceRoute:\n              projection: \"APPLICATION_INFERENCE_ROUTE_PROJECTION_ROUTE_NAME\"\n      storageBindings:",
        );
    fs::create_dir_all(root.join("catalog/applications"))
        .expect("route-name application directory");
    fs::write(root.join("catalog/applications/route-test.yaml"), source)
        .expect("route-name application fixture");
}

struct ApplicationHealthServer {
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl ApplicationHealthServer {
    fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ApplicationHealthServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn application_health_server() -> ApplicationHealthServer {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("Application readiness listener");
    let port = listener
        .local_addr()
        .expect("readiness listener address")
        .port();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
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
    ApplicationHealthServer { port, task }
}

async fn create_inference_route(backend: &support::RunningBackend) -> InferenceRoute {
    let mut models = backend.model_client();
    models
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: "fixture-route".to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        })
        .await
        .expect("load serving model");
    wait_for_inference_route(backend, "fixture-route").await
}

async fn create_route_test_application(
    client: &mut ApplicationClient,
    route: &InferenceRoute,
) -> ApplicationDeployment {
    client
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "route-test".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: Some(route.inference_route_id.clone()),
            expected_inference_route_generation: route.generation,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("create route-linked application")
        .into_inner()
        .deployment
        .expect("created application")
}

fn launched_route_name(
    runtime: &SimulatedRuntimeTestSupport,
    deployment_id: &str,
) -> Option<String> {
    runtime
        .group_launches()
        .into_iter()
        .rev()
        .find(|launch| launch.group_id.0 == deployment_id)
        .map(|launch| {
            launch.units[&dotcore_runtime::WorkloadUnitName("web".to_owned())].environment
                ["ROUTE_NAME"]
                .clone()
        })
}

fn group_calls_for(
    runtime: &SimulatedRuntimeTestSupport,
    deployment_id: &str,
) -> Vec<SimulatedRuntimeGroupCall> {
    runtime
        .group_calls()
        .into_iter()
        .filter(|call| call.group_id == deployment_id)
        .collect()
}

async fn wait_for_inference_route(
    backend: &support::RunningBackend,
    route_name: &str,
) -> InferenceRoute {
    let mut routes = backend.route_client();
    let mut listed = Vec::new();
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            listed = routes
                .list_routes(ListRoutesRequest {})
                .await
                .expect("list inference routes")
                .into_inner()
                .routes
                .into_iter()
                .filter_map(|view| view.route)
                .collect();
            if let Some(route) = listed.iter().find(|route| route.name == route_name) {
                return route.clone();
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("inference route {route_name}; last routes: {listed:?}"))
}

async fn create_application(client: &mut ApplicationClient) -> ApplicationDeployment {
    client
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("create application")
        .into_inner()
        .deployment
        .expect("created application")
}

async fn wait_for_application_route_name(
    client: &mut ApplicationClient,
    deployment_id: &str,
    route_name: &str,
) -> ApplicationDeployment {
    let mut last = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let deployment = client
                .get_application_deployment(GetApplicationDeploymentRequest {
                    application_deployment_id: deployment_id.to_owned(),
                })
                .await
                .expect("get route-bound application")
                .into_inner()
                .deployment
                .expect("route-bound application");
            if deployment
                .selected_inference_route
                .as_ref()
                .is_some_and(|selection| selection.route_name == route_name)
                && deployment.state.as_ref().is_some_and(|state| {
                    state.lifecycle_state == ApplicationLifecycleState::Ready as i32
                })
            {
                return deployment;
            }
            last = Some(deployment);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("application route launch projection; last deployment: {last:?}"))
}

async fn wait_for_application_lifecycle(
    client: &mut ApplicationClient,
    deployment_id: &str,
    lifecycle: ApplicationLifecycleState,
) -> ApplicationDeployment {
    wait_for_application_state(client, deployment_id, lifecycle, false).await
}

/// Waits like [`wait_for_application_lifecycle`], but only accepts a state the hosting node
/// reported since the backend started; after a restart, the previous run's record stays
/// projected, marked stale, until then.
async fn wait_for_fresh_application_lifecycle(
    client: &mut ApplicationClient,
    deployment_id: &str,
    lifecycle: ApplicationLifecycleState,
) -> ApplicationDeployment {
    wait_for_application_state(client, deployment_id, lifecycle, true).await
}

async fn wait_for_application_state(
    client: &mut ApplicationClient,
    deployment_id: &str,
    lifecycle: ApplicationLifecycleState,
    fresh: bool,
) -> ApplicationDeployment {
    let mut last_state = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let deployment = application_from_client(client, deployment_id).await;
            if deployment.state.as_ref().is_some_and(|state| {
                state.lifecycle_state == lifecycle as i32 && !(fresh && state.observation_stale)
            }) {
                return deployment;
            }
            last_state = deployment.state;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected application lifecycle state {lifecycle:?} (fresh observation: {fresh}); last state: {last_state:?}"
        )
    })
}

async fn wait_for_group_launches(runtime: &SimulatedRuntimeTestSupport, count: usize) {
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            if runtime.group_launches().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {count} simulated application group launches, observed {:?}",
            runtime.group_launches()
        )
    });
}

/// Renames the deployment, which republishes its node assignment and so wakes the hosting
/// node's reconciliation without changing its runtime identity.
async fn wake_node_reconciliation(client: &mut ApplicationClient, deployment_id: &str, name: &str) {
    client
        .update_application_deployment_metadata(UpdateApplicationDeploymentMetadataRequest {
            application_deployment_id: deployment_id.to_owned(),
            metadata: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                    display_name: Some(name.to_owned()),
                },
            ),
            update_mask: Some(FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        })
        .await
        .expect("renamed application");
}

async fn wait_for_group_calls(runtime: &SimulatedRuntimeTestSupport, count: usize) {
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            if runtime.group_calls().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "expected {count} simulated application runtime calls, observed {:?}",
            runtime.group_calls()
        )
    });
}

/// Waits until the simulated runtime records at least `count` group calls for one deployment
/// and returns them, reporting the last observed calls on timeout.
async fn wait_for_deployment_group_calls(
    runtime: &SimulatedRuntimeTestSupport,
    deployment_id: &str,
    count: usize,
) -> Vec<SimulatedRuntimeGroupCall> {
    let waited = tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let calls = group_calls_for(runtime, deployment_id);
            if calls.len() >= count {
                return calls;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    waited.unwrap_or_else(|_| {
        panic!(
            "expected {count} simulated runtime calls for {deployment_id}, observed {:?}",
            group_calls_for(runtime, deployment_id)
        )
    })
}

async fn wait_for_application_removal(client: &mut ApplicationClient) {
    let mut remaining = Vec::new();
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            remaining = client
                .list_application_deployments(ListApplicationDeploymentsRequest {})
                .await
                .expect("application deployment list")
                .into_inner()
                .deployments;
            if remaining.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("application deployment removal; last deployments: {remaining:?}"));
}

async fn application_from_client(
    client: &mut ApplicationClient,
    deployment_id: &str,
) -> ApplicationDeployment {
    client
        .get_application_deployment(GetApplicationDeploymentRequest {
            application_deployment_id: deployment_id.to_owned(),
        })
        .await
        .expect("Application deployment")
        .into_inner()
        .deployment
        .expect("Application deployment")
}

/// Returns the data one application deployment's launch attaches under its deployment scope.
fn deployment_data(deployment_id: &str) -> dotcore_runtime::WorkloadStorageKey {
    dotcore_runtime::WorkloadStorageKey {
        kind: dotcore_runtime::WorkloadStorageKind::Data,
        namespace: "deployment".to_owned(),
        key: deployment_id.to_owned(),
    }
}

fn assert_application_ready(deployment: &ApplicationDeployment, port: u16) {
    let state = deployment.state.as_ref().expect("Application state");
    assert_eq!(
        deployment.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Ready as i32
    );
    assert_eq!(
        state.readiness_state,
        ApplicationReadinessState::Ready as i32
    );
    assert_eq!(state.stage, "ready");
    assert_eq!(
        state
            .endpoint
            .as_ref()
            .map(|endpoint| endpoint.url.as_str()),
        Some(format!("http://127.0.0.1:{port}").as_str())
    );
    assert_eq!(state.services.len(), 1);
    assert_eq!(state.services[0].service_name, "web");
}

fn assert_application_suppressed(
    deployment: &ApplicationDeployment,
    lifecycle: ApplicationLifecycleState,
) {
    let state = deployment.state.as_ref().expect("Application state");
    assert_eq!(
        deployment.desired_state,
        ApplicationDeploymentDesiredState::Paused as i32
    );
    assert_eq!(state.lifecycle_state, lifecycle as i32);
    assert_eq!(
        state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert!(state.endpoint.is_none());
    assert!(state.services.is_empty());
}

fn assert_application_starting(deployment: &ApplicationDeployment) {
    let state = deployment.state.as_ref().expect("Application state");
    assert_eq!(
        deployment.desired_state,
        ApplicationDeploymentDesiredState::Present as i32
    );
    assert_eq!(
        state.lifecycle_state,
        ApplicationLifecycleState::Starting as i32
    );
    assert_eq!(
        state.readiness_state,
        ApplicationReadinessState::NotReady as i32
    );
    assert!(state.endpoint.is_none());
    assert!(state.services.is_empty());
}

fn application_declaration(root: &Path, deployment_id: &str) -> ApplicationDeploymentDeclaration {
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("desired site");
    site.application_deployments
        .into_iter()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
        .expect("Application declaration")
}

fn application_current(root: &Path, deployment_id: &str) -> CurrentApplicationDeployment {
    let state = SiteCurrentState::decode(
        fs::read(root.join("state.pb"))
            .expect("current state")
            .as_slice(),
    )
    .expect("current state protobuf");
    state
        .application_deployments
        .into_iter()
        .find(|deployment| deployment.application_deployment_id == deployment_id)
        .expect("Application current record")
}

fn application_declaration_count(root: &Path) -> usize {
    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("desired site");
    site.application_deployments.len()
}

async fn deployment_from_client(client: &mut ModelClient, deployment_id: &str) -> ModelDeployment {
    client
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("model deployment list")
        .into_inner()
        .deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .expect("model deployment")
}

async fn wait_for_lifecycle_client(
    client: &mut ModelClient,
    deployment_id: &str,
    lifecycle: ModelDeploymentStatus,
) -> ModelDeployment {
    let mut last_state = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let deployment = deployment_from_client(client, deployment_id).await;
            if deployment
                .state
                .as_ref()
                .is_some_and(|state| state.status == lifecycle as i32)
            {
                return deployment;
            }
            last_state = deployment.state;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("expected model lifecycle state {lifecycle:?}; last state: {last_state:?}")
    })
}

async fn wait_for_deployment_removal_client(client: &mut ModelClient) {
    let mut remaining = Vec::new();
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            remaining = client
                .list_model_deployments(ListModelDeploymentsRequest {})
                .await
                .expect("model deployment list")
                .into_inner()
                .deployments;
            if remaining.is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("model deployment removal; last deployments: {remaining:?}"));
}

async fn wait_for_simulated_stop_failure(runtime: &SimulatedRuntimeTestSupport) {
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            if runtime.call_counts().failed_stops == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "simulated stop failure; last call counts: {:?}",
            runtime.call_counts()
        )
    });
}

fn state_record(root: &std::path::Path, deployment_id: &str) -> ModelDeploymentState {
    let bytes = fs::read(root.join("state.pb")).expect("current state");
    let state = SiteCurrentState::decode(bytes.as_slice()).expect("current state protobuf");
    state
        .model_deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .and_then(|deployment| deployment.state)
        .expect("Model current state record")
}

/// Waits until the retained record of `deployment_id` in `state.pb` reaches `status`.
async fn wait_for_state_record_status(
    root: &std::path::Path,
    deployment_id: &str,
    status: ModelDeploymentStatus,
) {
    let mut last = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            let record = state_record(root, deployment_id);
            if record.status == status as i32 {
                return;
            }
            last = Some(record);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("retained record status {status:?}; last record: {last:?}"));
}

fn model_metadata_request(
    model_deployment_id: String,
    display_name: &str,
) -> UpdateModelDeploymentMetadataRequest {
    UpdateModelDeploymentMetadataRequest {
        model_deployment_id,
        metadata: Some(ModelDeploymentMetadata {
            display_name: Some(display_name.to_owned()),
        }),
        update_mask: Some(FieldMask {
            paths: vec!["display_name".to_owned()],
        }),
    }
}

fn model_declaration(root: &std::path::Path, deployment_id: &str) -> ModelDeploymentDeclaration {
    let site: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("declarative site");
    site.model_deployments
        .into_iter()
        .find(|deployment| deployment.model_deployment_id == deployment_id)
        .expect("model declaration")
}

fn assert_model_declaration(
    root: &std::path::Path,
    deployment: &ModelDeployment,
    desired_state: ModelDeploymentDesiredState,
) {
    let declaration = model_declaration(root, &deployment.model_deployment_id);
    let intent = deployment.intent.as_ref().expect("model deployment intent");
    assert_eq!(
        declaration.model_deployment_id,
        deployment.model_deployment_id
    );
    assert_eq!(declaration.placement, intent.placement);
    assert_eq!(
        declaration.requested_parallel_shape,
        intent.requested_parallel_shape
    );
    assert_eq!(declaration.snapshot, deployment.snapshot);
    assert_eq!(declaration.desired_state, desired_state as i32);
}

fn local_snapshot(
    snapshot: &dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot,
) -> &dotcore_proto::dotcore::model::v1::ResolvedLocalModelDeploymentSnapshot {
    match snapshot.source.as_ref() {
        Some(ModelDeploymentSnapshotSource::Local(local)) => local,
        Some(ModelDeploymentSnapshotSource::Upstream(_)) | None => panic!("local model snapshot"),
    }
}

fn local_snapshot_model_id(
    snapshot: &dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot,
) -> &str {
    &local_snapshot(snapshot).model_id
}

async fn served_models(address: SocketAddr) -> serde_json::Value {
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("Inference router connection");
    stream
        .write_all(
            format!(
                "GET /v1/models HTTP/1.1\r\nHost: {address}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("Inference router request");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("Inference router response");
    let response = String::from_utf8(response).expect("Inference response UTF-8");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("Inference response body");
    serde_json::from_str(body).expect("Inference model list JSON")
}

async fn served_chat(address: SocketAddr, model_id: &str) -> serde_json::Value {
    let body = format!(
        "{{\"model\":\"{model_id}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hello\"}}]}}"
    );
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("Inference router connection");
    stream
        .write_all(
            format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .expect("Inference router request");
    let mut response = Vec::new();
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        stream.read_to_end(&mut response),
    )
    .await
    .expect("Inference router response timeout")
    .expect("Inference router response");
    let response = String::from_utf8(response).expect("Inference response UTF-8");
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("Inference response body");
    serde_json::from_str(body).expect("Inference Chat JSON")
}

#[tokio::test]
#[ignore = "hardware: just backend::test-docker"]
async fn hw_docker_simulated_fixture_loads_its_simulated_model_ready() {
    let root = support::temporary_simulated_docker_fixture();
    let service = model_service_for(&support::configuration(&root))
        .await
        .expect("simulated-docker model service");

    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement(
                            "simulated-docker-docker",
                            "simulated-docker-target",
                        )),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load simulated-docker model")
    .into_inner()
    .deployment
    .expect("accepted simulated-docker deployment");
    let deployment_id = deployment.model_deployment_id.clone();
    assert_eq!(
        deployment.desired_state,
        ModelDeploymentDesiredState::Present as i32
    );
    let snapshot = deployment
        .snapshot
        .as_ref()
        .expect("simulated-docker immutable snapshot");
    assert_eq!(local_snapshot_model_id(snapshot), "fixture-model");
    assert_eq!(local_snapshot(snapshot).profile_id, "fixture-profile");
    assert_eq!(
        local_snapshot(snapshot)
            .engine_artifact
            .as_ref()
            .expect("simulated-docker engine artifact")
            .engine_release_id,
        "fixture-engine"
    );

    let second_deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model-two".to_owned(),
                        profile_id: "fixture-profile-two".to_owned(),
                        placement: Some(support::exact_placement(
                            "simulated-docker-docker",
                            "simulated-docker-target",
                        )),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact-two",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load second simulated-docker model")
    .into_inner()
    .deployment
    .expect("accepted second simulated-docker deployment");
    assert_eq!(
        local_snapshot_model_id(
            second_deployment
                .snapshot
                .as_ref()
                .expect("second immutable snapshot"),
        ),
        "fixture-model-two"
    );

    service.start_observer();
    wait_for_lifecycle(&service, &deployment_id, ModelDeploymentStatus::Ready).await;
}

#[tokio::test]
async fn application_restart_uses_site_and_state_ignores_obsolete_files_and_embedded_catalog() {
    let root = support::temporary_fast_fixture();
    let configuration = application_lifecycle_configuration(&root);
    let service = application_service_for(&configuration)
        .await
        .expect("initial application service");
    let created = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create application")
    .into_inner()
    .deployment
    .expect("created application");
    drop(service);

    fs::write(
        root.join("applications-desired.pb"),
        b"corrupt obsolete desired",
    )
    .expect("obsolete desired file");
    fs::write(
        root.join("applications-current.pb"),
        b"corrupt obsolete current",
    )
    .expect("obsolete current file");
    let empty_catalog = root.join("empty-platform-catalog");
    fs::create_dir(&empty_catalog).expect("empty catalog root");
    fs::create_dir(empty_catalog.join("models")).expect("empty model catalog");
    fs::create_dir(empty_catalog.join("engines")).expect("empty engine catalog");
    let restarted_configuration = dotcore_domain::SitePaths::from_values(
        Some(root.to_str().expect("Site root")),
        Some(empty_catalog.to_str().expect("Catalog root")),
        Some(root.join("cache").to_str().expect("artifact cache")),
    )
    .expect("restart configuration");

    let restarted = application_service_for(&restarted_configuration)
        .await
        .expect("restart without catalog application");
    let deployments = ApplicationServiceContract::list_application_deployments(
        &restarted,
        Request::new(ListApplicationDeploymentsRequest {}),
    )
    .await
    .expect("restart deployment projection")
    .into_inner()
    .deployments;
    assert_eq!(deployments.len(), 1);
    assert_eq!(
        deployments[0].application_deployment_id,
        created.application_deployment_id
    );
    assert_eq!(deployments[0].snapshot, created.snapshot);
}

#[tokio::test]
async fn current_only_application_state_never_creates_a_public_deployment() {
    let root = support::temporary_fast_fixture();
    std::fs::create_dir(root.join("node")).expect("node store");
    let current = FileCurrentState::open(root.join("site.yaml"), "fixture-site")
        .expect("shared current state");
    CurrentApplicationRepository::upsert(
        &current,
        CurrentApplicationDeployment {
            application_deployment_id: "current-only-260812t120000z-000001".to_owned(),
            record_revision: 0,
            state: Some(ApplicationDeploymentState {
                lifecycle_state: ApplicationLifecycleState::Running as i32,
                readiness_state: ApplicationReadinessState::NotReady as i32,
                stage: "running".to_owned(),
                ..Default::default()
            }),
            reconciled_lifecycle_generation: None,
        },
        None,
    )
    .expect("current-only record");
    drop(current);

    let service = application_service_for(&support::configuration(&root))
        .await
        .expect("Application service");
    let deployments = ApplicationServiceContract::list_application_deployments(
        &service,
        Request::new(ListApplicationDeploymentsRequest {}),
    )
    .await
    .expect("Application projection")
    .into_inner()
    .deployments;
    assert!(deployments.is_empty());
}

#[tokio::test]
async fn application_service_persists_its_snapshot_name_and_logical_storage_until_removal() {
    let root = support::temporary_fast_fixture();
    let service = application_service_for(&application_lifecycle_configuration(&root))
        .await
        .expect("Application service");

    let created = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create application deployment")
    .into_inner()
    .deployment
    .expect("created application deployment");
    assert_eq!(created.application_deployment_name, "Open WebUI");
    assert!(created.application_deployment_id.starts_with("open-webui-"));
    assert_eq!(created.storage_attachments[0].storage_name, "web-data");
    assert_eq!(
        created
            .snapshot
            .as_ref()
            .expect("immutable application snapshot")
            .application_id,
        "open-webui"
    );

    service.start_observer();
    let mut last_state = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            last_state = ApplicationServiceContract::list_application_deployments(
                &service,
                Request::new(ListApplicationDeploymentsRequest {}),
            )
            .await
            .expect("application deployment list")
            .into_inner()
            .deployments
            .into_iter()
            .find(|deployment| {
                deployment.application_deployment_id == created.application_deployment_id
            })
            .and_then(|deployment| deployment.state);
            if last_state.as_ref().is_some_and(|state| {
                state.lifecycle_state == ApplicationLifecycleState::Ready as i32
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "simulated deployment is reported as ready without replacement; \
             last state: {last_state:?}"
        )
    });

    let removed = ApplicationServiceContract::remove_application_deployment(
        &service,
        Request::new(RemoveApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id,
        }),
    )
    .await
    .expect("remove application deployment")
    .into_inner()
    .deployment
    .expect("removed application deployment");
    assert_eq!(removed.desired_state, 2);
}

#[tokio::test]
async fn application_service_assigns_timestamped_ids_while_permitting_duplicate_display_names() {
    let root = support::temporary_fast_fixture();
    let service = application_service_for(&application_lifecycle_configuration(&root))
        .await
        .expect("Application service");

    let first = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create first application deployment")
    .into_inner()
    .deployment
    .expect("first application deployment");
    let second = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create second application deployment")
    .into_inner()
    .deployment
    .expect("second application deployment");

    assert_eq!(first.application_deployment_name, "Open WebUI");
    assert_eq!(second.application_deployment_name, "Open WebUI");
    assert_ne!(
        first.application_deployment_id,
        second.application_deployment_id
    );
    for deployment_id in [
        &first.application_deployment_id,
        &second.application_deployment_id,
    ] {
        assert!(deployment_id.starts_with("open-webui-"));
        let (_, suffix) = deployment_id
            .rsplit_once('-')
            .expect("deployment ID suffix");
        assert_eq!(suffix.len(), 10);
        assert!(
            suffix
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        );
    }

    let site = fs::read_to_string(root.join("site.yaml")).expect("declarative site");
    assert_eq!(
        site.matches("applicationDeploymentName: Open WebUI")
            .count(),
        2
    );
    assert!(site.contains(&format!(
        "applicationDeploymentId: {}",
        first.application_deployment_id
    )));
    assert!(site.contains(&format!(
        "applicationDeploymentId: {}",
        second.application_deployment_id
    )));
}

#[tokio::test]
async fn model_metadata_update_persists_and_projects_through_service_reopen() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    let created = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("create model deployment")
    .into_inner()
    .deployment
    .expect("created model deployment");

    let renamed = ModelServiceContract::update_model_deployment_metadata(
        &service,
        Request::new(model_metadata_request(
            created.model_deployment_id.clone(),
            "  Model — renamed  ",
        )),
    )
    .await
    .expect("rename model deployment")
    .into_inner()
    .deployment
    .expect("renamed model deployment");
    assert_eq!(renamed.model_deployment_name, "Model — renamed");

    let persisted: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("persisted site");
    assert_eq!(
        persisted.model_deployments,
        vec![ModelDeploymentDeclaration {
            model_deployment_name: "Model — renamed".to_owned(),
            ..model_declaration(&root, &created.model_deployment_id)
        }]
    );

    drop(service);
    let reopened = model_service_for(&configuration)
        .await
        .expect("reopened model service");
    let deployment = ModelServiceContract::list_model_deployments(
        &reopened,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("reopened model deployment projection")
    .into_inner()
    .deployments
    .into_iter()
    .find(|deployment| deployment.model_deployment_id == created.model_deployment_id)
    .expect("reopened renamed model deployment");
    assert_eq!(deployment.model_deployment_name, "Model — renamed");
}

#[tokio::test]
async fn application_metadata_update_persists_and_projects_through_service_reopen() {
    let root = support::temporary_fast_fixture();
    let configuration = application_lifecycle_configuration(&root);
    let service = application_service_for(&configuration)
        .await
        .expect("Application service");
    let created = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create application deployment")
    .into_inner()
    .deployment
    .expect("created application deployment");

    let renamed = ApplicationServiceContract::update_application_deployment_metadata(
        &service,
        Request::new(UpdateApplicationDeploymentMetadataRequest {
            application_deployment_id: created.application_deployment_id.clone(),
            metadata: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                    display_name: Some("  App — renamed  ".to_owned()),
                },
            ),
            update_mask: Some(dotcore_proto::google::protobuf::FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        }),
    )
    .await
    .expect("rename application deployment")
    .into_inner()
    .deployment
    .expect("renamed application deployment");
    assert_eq!(renamed.application_deployment_name, "App — renamed");

    let persisted: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("persisted site");
    assert_eq!(
        persisted.application_deployments,
        vec![ApplicationDeploymentDeclaration {
            application_deployment_name: "App — renamed".to_owned(),
            ..application_declaration(&root, &created.application_deployment_id)
        }]
    );

    drop(service);
    let reopened = application_service_for(&configuration)
        .await
        .expect("reopened application service");
    let deployment = ApplicationServiceContract::list_application_deployments(
        &reopened,
        Request::new(ListApplicationDeploymentsRequest {}),
    )
    .await
    .expect("reopened application deployment projection")
    .into_inner()
    .deployments
    .into_iter()
    .find(|deployment| deployment.application_deployment_id == created.application_deployment_id)
    .expect("reopened renamed application deployment");
    assert_eq!(deployment.application_deployment_name, "App — renamed");
}

#[tokio::test]
async fn application_metadata_update_persists_same_name_and_uncertain_acceptance_without_execution()
{
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_application_endpoint(0);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        application_lifecycle_configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.application_client();
    let created = client
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: Some("Stable Application".to_owned()),
            parameter_values: Vec::new(),
        })
        .await
        .expect("create application")
        .into_inner()
        .deployment
        .expect("created application");
    // The node's settled status fixes the current record before metadata is compared.
    wait_for_application_lifecycle(
        &mut client,
        &created.application_deployment_id,
        ApplicationLifecycleState::Ready,
    )
    .await;
    let state_before = fs::read(root.join("state.pb")).expect("current state before metadata");
    let calls_before = runtime.group_calls();
    let request = |name: &str| UpdateApplicationDeploymentMetadataRequest {
        application_deployment_id: created.application_deployment_id.clone(),
        metadata: Some(
            dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                display_name: Some(name.to_owned()),
            },
        ),
        update_mask: Some(FieldMask {
            paths: vec!["display_name".to_owned()],
        }),
    };
    let first = client
        .update_application_deployment_metadata(request("Stable Application"))
        .await
        .expect("same-name application metadata update")
        .into_inner()
        .deployment
        .expect("same-name projection");
    let second = client
        .update_application_deployment_metadata(request("Stable Application"))
        .await
        .expect("repeated same-name application metadata update")
        .into_inner()
        .deployment
        .expect("repeated same-name projection");
    assert_eq!(first, second);
    assert_eq!(first.application_deployment_name, "Stable Application");
    assert_eq!(
        fs::read(root.join("state.pb")).expect("current state after metadata"),
        state_before
    );
    assert_eq!(runtime.group_calls(), calls_before);
    assert_eq!(
        application_declaration(&root, &created.application_deployment_id)
            .application_deployment_name,
        "Stable Application"
    );

    drop(client);
    backend.stop().await;
    // Composition first waits until the stopped backend released the site stores.
    let reopened = application_service_for(&support::configuration(&root))
        .await
        .expect("reopened application service");
    let persisted = ApplicationServiceContract::get_application_deployment(
        &reopened,
        Request::new(GetApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id,
        }),
    )
    .await
    .expect("reopened application projection")
    .into_inner()
    .deployment
    .expect("reopened application");
    assert_eq!(persisted.application_deployment_name, "Stable Application");
}

#[tokio::test]
async fn application_metadata_update_acceptance_order_is_deterministic_against_teardown() {
    let root = support::temporary_fast_fixture();
    let service = application_service_for(&application_lifecycle_configuration(&root))
        .await
        .expect("Application service");
    let created = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create application")
    .into_inner()
    .deployment
    .expect("created application");
    let update = |name: &str| UpdateApplicationDeploymentMetadataRequest {
        application_deployment_id: created.application_deployment_id.clone(),
        metadata: Some(
            dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                display_name: Some(name.to_owned()),
            },
        ),
        update_mask: Some(FieldMask {
            paths: vec!["display_name".to_owned()],
        }),
    };
    let accepted = ApplicationServiceContract::update_application_deployment_metadata(
        &service,
        Request::new(update("accepted before teardown")),
    )
    .await
    .expect("metadata update before teardown")
    .into_inner()
    .deployment
    .expect("updated application");
    assert_eq!(
        accepted.application_deployment_name,
        "accepted before teardown"
    );
    let removed = ApplicationServiceContract::remove_application_deployment(
        &service,
        Request::new(RemoveApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        }),
    )
    .await
    .expect("teardown after metadata update")
    .into_inner()
    .deployment
    .expect("teardown application");
    assert_eq!(
        removed.application_deployment_name,
        "accepted before teardown"
    );
    assert_eq!(
        removed.desired_state,
        ApplicationDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(
        ApplicationServiceContract::update_application_deployment_metadata(
            &service,
            Request::new(update("rejected after teardown")),
        )
        .await
        .expect_err("metadata update after teardown")
        .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(
        application_declaration(&root, &created.application_deployment_id)
            .application_deployment_name,
        "accepted before teardown"
    );

    let root = support::temporary_fast_fixture();
    let service = application_service_for(&application_lifecycle_configuration(&root))
        .await
        .expect("second application service");
    let created = ApplicationServiceContract::create_application_deployment(
        &service,
        Request::new(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        }),
    )
    .await
    .expect("create second application")
    .into_inner()
    .deployment
    .expect("created second application");
    ApplicationServiceContract::remove_application_deployment(
        &service,
        Request::new(RemoveApplicationDeploymentRequest {
            application_deployment_id: created.application_deployment_id.clone(),
        }),
    )
    .await
    .expect("teardown before metadata update");
    let error = ApplicationServiceContract::update_application_deployment_metadata(
        &service,
        Request::new(UpdateApplicationDeploymentMetadataRequest {
            application_deployment_id: created.application_deployment_id.clone(),
            metadata: Some(
                dotcore_proto::dotcore::application::v1::ApplicationDeploymentMetadata {
                    display_name: Some("rejected after teardown".to_owned()),
                },
            ),
            update_mask: Some(FieldMask {
                paths: vec!["display_name".to_owned()],
            }),
        }),
    )
    .await
    .expect_err("metadata update after teardown");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_ne!(
        application_declaration(&root, &created.application_deployment_id)
            .application_deployment_name,
        "rejected after teardown"
    );
}

#[tokio::test]
async fn ordered_concurrent_model_metadata_updates_keep_the_later_accepted_name_without_execution_side_effects()
 {
    let root = support::temporary_fast_fixture();
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        support::configuration(&root),
        runtime.clone(),
    )
    .await;
    let mut client = backend.model_client();
    let created = client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("create model deployment")
        .into_inner()
        .deployment
        .expect("created model deployment");
    wait_for_lifecycle_client(
        &mut client,
        &created.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    let calls_before = runtime.call_counts();

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let first_accepted = std::sync::Arc::new(tokio::sync::Notify::new());
    let first = {
        let mut client = backend.model_client();
        let barrier = std::sync::Arc::clone(&barrier);
        let first_accepted = std::sync::Arc::clone(&first_accepted);
        let deployment_id = created.model_deployment_id.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            let response = client
                .update_model_deployment_metadata(model_metadata_request(
                    deployment_id,
                    "first accepted name",
                ))
                .await
                .expect("first metadata update")
                .into_inner()
                .deployment
                .expect("first accepted deployment");
            first_accepted.notify_one();
            response
        })
    };
    let second = {
        let mut client = backend.model_client();
        let barrier = std::sync::Arc::clone(&barrier);
        let first_accepted = std::sync::Arc::clone(&first_accepted);
        let deployment_id = created.model_deployment_id.clone();
        tokio::spawn(async move {
            barrier.wait().await;
            first_accepted.notified().await;
            client
                .update_model_deployment_metadata(model_metadata_request(
                    deployment_id,
                    "later accepted name",
                ))
                .await
                .expect("later metadata update")
                .into_inner()
                .deployment
                .expect("later accepted deployment")
        })
    };
    assert_eq!(
        first
            .await
            .expect("first metadata update task")
            .model_deployment_name,
        "first accepted name"
    );
    assert_eq!(
        second
            .await
            .expect("later metadata update task")
            .model_deployment_name,
        "later accepted name"
    );

    let projected = deployment_from_client(&mut client, &created.model_deployment_id).await;
    assert_eq!(projected.model_deployment_name, "later accepted name");
    let in_memory_projection = client
        .list_model_deployments(ListModelDeploymentsRequest {})
        .await
        .expect("shared in-memory site projection")
        .into_inner()
        .deployments;
    assert_eq!(
        in_memory_projection
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == created.model_deployment_id)
            .expect("shared in-memory deployment")
            .model_deployment_name,
        "later accepted name"
    );
    assert_eq!(
        model_declaration(&root, &created.model_deployment_id).model_deployment_name,
        "later accepted name"
    );
    assert_eq!(runtime.call_counts(), calls_before);

    backend.stop().await;
    let reopened = model_service_for(&support::configuration(&root))
        .await
        .expect("reopened model service");
    let deployment = ModelServiceContract::list_model_deployments(
        &reopened,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("reopened model projection")
    .into_inner()
    .deployments
    .into_iter()
    .find(|deployment| deployment.model_deployment_id == created.model_deployment_id)
    .expect("reopened deployment");
    assert_eq!(deployment.model_deployment_name, "later accepted name");
}

#[tokio::test]
async fn model_metadata_update_acceptance_order_is_deterministic_against_teardown() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    let created = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("create model deployment")
    .into_inner()
    .deployment
    .expect("created model deployment");
    let state_before_update = fs::read(root.join("state.pb")).expect("current state before update");

    let updated = ModelServiceContract::update_model_deployment_metadata(
        &service,
        Request::new(model_metadata_request(
            created.model_deployment_id.clone(),
            "accepted before teardown",
        )),
    )
    .await
    .expect("metadata update before teardown")
    .into_inner()
    .deployment
    .expect("updated model deployment");
    assert_eq!(updated.model_deployment_name, "accepted before teardown");
    assert_eq!(
        fs::read(root.join("state.pb")).expect("current state after update"),
        state_before_update,
        "metadata updates must not write current state"
    );

    let removed = ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: created.model_deployment_id.clone(),
        }),
    )
    .await
    .expect("teardown after metadata update")
    .into_inner()
    .deployment
    .expect("teardown deployment");
    assert_eq!(
        removed.desired_state,
        ModelDeploymentDesiredState::Teardown as i32
    );
    assert_eq!(removed.model_deployment_name, "accepted before teardown");

    let error = ModelServiceContract::update_model_deployment_metadata(
        &service,
        Request::new(model_metadata_request(
            created.model_deployment_id.clone(),
            "rejected after teardown",
        )),
    )
    .await
    .expect_err("metadata update after teardown");
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(
        model_declaration(&root, &created.model_deployment_id).model_deployment_name,
        "accepted before teardown"
    );
}

#[tokio::test]
async fn interleaved_model_and_application_mutations_survive_canonical_site_rewrite() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(application_lifecycle_configuration(&root)).await;
    let mut model_client = backend.model_client();
    let mut application_client = backend.application_client();
    let model = model_client
        .create_model_deployment(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        })
        .await
        .expect("Model mutation")
        .into_inner()
        .deployment
        .expect("model deployment");
    let application = application_client
        .create_application_deployment(CreateApplicationDeploymentRequest {
            application_id: "open-webui".to_owned(),
            placement: Some(support::exact_placement(
                "fixture-node",
                "fixture-application-target",
            )),
            inference_route_id: None,
            expected_inference_route_generation: 0,
            application_deployment_name: None,
            parameter_values: Vec::new(),
        })
        .await
        .expect("Application mutation")
        .into_inner()
        .deployment
        .expect("Application deployment");

    let site: Site = dotcore_proto::yaml::from_file(&root.join("site.yaml"))
        .expect("canonical interleaved site");
    assert_eq!(site.model_deployments.len(), 1);
    assert_eq!(
        site.model_deployments[0].model_deployment_id,
        model.model_deployment_id
    );
    assert_eq!(site.application_deployments.len(), 1);
    assert_eq!(
        site.application_deployments[0].application_deployment_id,
        application.application_deployment_id
    );
    backend.stop().await;
}

#[tokio::test]
async fn services_keep_catalog_reads_and_snapshot_backed_deployments_separate() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let catalog = catalog_service_for(&configuration).expect("Catalog service");
    let models = CatalogServiceContract::list_models(&catalog, Request::new(ListModelsRequest {}))
        .await
        .expect("Catalog read")
        .into_inner();
    assert!(
        models
            .models
            .iter()
            .any(|model| model.model_id == "fixture-model")
    );
    let engine_releases = CatalogServiceContract::list_engine_releases(
        &catalog,
        Request::new(ListEngineReleasesRequest {}),
    )
    .await
    .expect("Catalog engine release read")
    .into_inner();
    let fixture_engine = engine_releases
        .engine_releases
        .iter()
        .find(|release| release.engine_release_id == "fixture-engine")
        .expect("fixture engine release");
    assert_eq!(
        fixture_engine
            .parameter_schema
            .as_ref()
            .expect("fixture engine parameter schema")
            .parameters[0]
            .display_name,
        "Total context"
    );
    let applications = CatalogServiceContract::list_applications(
        &catalog,
        Request::new(ListApplicationsRequest {}),
    )
    .await
    .expect("Catalog application read")
    .into_inner();
    assert!(
        applications
            .applications
            .iter()
            .all(|application| application.graph.is_some())
    );

    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    let before = ModelServiceContract::list_model_deployments(
        &service,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("deployment read")
    .into_inner();
    assert!(
        before
            .deployments
            .iter()
            .all(|deployment| deployment.snapshot.is_some())
    );

    let loaded = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");
    let snapshot = loaded.snapshot.as_ref().expect("immutable snapshot");
    assert_eq!(local_snapshot_model_id(snapshot), "fixture-model");
    assert_eq!(local_snapshot(snapshot).profile_id, "fixture-profile");
    assert_eq!(
        local_snapshot(snapshot)
            .engine_artifact
            .as_ref()
            .expect("engine artifact")
            .engine_release_id,
        "fixture-engine"
    );
    assert!(
        fs::read_to_string(root.join("site.yaml"))
            .expect("desired site")
            .contains("snapshot:")
    );
    drop(service);

    let restarted = model_service_for(&configuration).await.expect("restart");
    let deployments = ModelServiceContract::list_model_deployments(
        &restarted,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("deployment read after restart")
    .into_inner();
    assert!(deployments.deployments.iter().any(|deployment| {
        deployment.model_deployment_id == loaded.model_deployment_id
            && deployment.snapshot == loaded.snapshot
    }));
    ModelServiceContract::remove_model_deployment(
        &restarted,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id,
        }),
    )
    .await
    .expect("unload");
    drop(restarted);
}

#[tokio::test]
async fn settled_observer_releases_site_store_when_services_are_dropped() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();

    tokio::task::yield_now().await;
    ModelServiceContract::list_model_deployments(
        &service,
        Request::new(ListModelDeploymentsRequest {}),
    )
    .await
    .expect("settled observer read");
    tokio::task::yield_now().await;
    drop(service);

    let mut last_error = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            match FileCurrentState::open(root.join("site.yaml"), "fixture-site") {
                Ok(state) => break state,
                Err(error) if error.to_string().contains("already locked") => {
                    last_error = Some(error.to_string());
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                Err(error) => panic!("reopen site store: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("observer released site store; last error: {last_error:?}"));
}

#[test]
fn nvidia_site_declares_the_exact_nvidia_model_target() {
    let path = support::workspace_root().join("backend/fixtures/nvidia-site/site.yaml");
    let site: Site = dotcore_proto::yaml::from_file(&path).expect("NVIDIA site fixture");

    assert_eq!(site.site_id, "development-site");
    assert_eq!(site.nodes.len(), 1);
    assert_eq!(site.nodes[0].node_id, "development-node");
    assert_eq!(site.nodes[0].display_name, "Development node");
    let target = site
        .execution_targets
        .iter()
        .find(|target| target.execution_target_id == "nvidia-target")
        .expect("NVIDIA model execution target");
    assert_eq!(target.site_node_id, "development-node");
    assert_eq!(target.accelerator_device_ids, vec!["nvidia.com/gpu=0"]);
    assert!(matches!(
        target.runtime_binding.as_ref(),
        Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ModelRuntime(binding))
            if matches!(
                binding.adapter.as_ref(),
                Some(dotcore_proto::dotcore::site::v1::model_runtime_binding::Adapter::DockerRuntime(settings))
                    if settings.accelerator_transport
                        == dotcore_proto::dotcore::site::v1::DockerAcceleratorTransport::Cdi as i32
            )
    ));
    assert!(site.model_deployments.is_empty());
}

#[tokio::test]
async fn unload_retains_absent_snapshot_through_restart_and_catalog_removal() {
    let root = support::temporary_fast_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    fs::write(
        &site_path,
        source.replace(
            FAST_TEST_SCENARIO,
            "defaultScenario:\n        readyDelayMillis: 0\n        stopDelayMillis: 1000",
        ),
    )
    .expect("stop-delay fixture");
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();
    let loaded = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");

    wait_for_lifecycle(
        &service,
        &loaded.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    let unloading = ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: loaded.model_deployment_id.clone(),
        }),
    )
    .await
    .expect("unload")
    .into_inner()
    .deployment
    .expect("unloading deployment");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(unloading.desired_state),
        Ok(ModelDeploymentDesiredState::Teardown)
    );
    assert_eq!(
        unloading.state.expect("unloading state").status,
        ModelDeploymentStatus::Unloading as i32
    );
    let retained: Site = dotcore_proto::yaml::from_file(&site_path).expect("retained desired site");
    let retained = retained
        .model_deployments
        .iter()
        .find(|deployment| deployment.model_deployment_id == loaded.model_deployment_id)
        .expect("retained teardown intent");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(retained.desired_state),
        Ok(ModelDeploymentDesiredState::Teardown)
    );
    assert_eq!(retained.placement, loaded.intent.expect("intent").placement);
    assert_eq!(retained.snapshot, loaded.snapshot);

    drop(service);
    support::wait_for_site_store_release(&configuration).await;
    fs::remove_file(root.join("catalog/models/fixture-model.yaml")).expect("remove catalog model");
    let restarted = model_service_for(&configuration)
        .await
        .expect("restart without catalog model");
    restarted.start_observer();
    wait_for_deployment_removal(&restarted).await;
    let complete: Site =
        dotcore_proto::yaml::from_file(&site_path).expect("completed desired site");
    assert!(complete.model_deployments.is_empty());
    drop(restarted);
}

#[tokio::test]
async fn simulated_stop_delay_uses_the_deployment_override() {
    // The fixture's default stop takes an hour, so removal within the convergence deadline
    // proves the deployment's zero-delay override applied.
    let root = support::temporary_fast_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    fs::write(
        &site_path,
        source.replace(
            FAST_TEST_SCENARIO,
            "defaultScenario:\n        readyDelayMillis: 0\n        stopDelayMillis: 3600000\n      deploymentScenarios:\n      - modelDeploymentId: model-d44bc5d823b11591-1\n        scenario:\n          stopDelayMillis: 0",
        ),
    )
    .expect("stop-delay fixture");
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");
    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        }),
    )
    .await
    .expect("unload");
    wait_for_deployment_removal(&service).await;
    drop(service);
}

#[tokio::test]
async fn simulated_stop_delay_converges_after_repeated_unloading_observations() {
    let root = support::temporary_fast_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    fs::write(
        &site_path,
        source.replace(
            FAST_TEST_SCENARIO,
            "defaultScenario:\n        readyDelayMillis: 0\n        stopDelayMillis: 40",
        ),
    )
    .expect("stop-delay fixture");
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");
    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id,
        }),
    )
    .await
    .expect("unload");
    wait_for_deployment_removal(&service).await;
    drop(service);
}

#[tokio::test]
async fn repeated_simulated_lifecycles_converge_without_retaining_teardown_state() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();

    for _ in 0..3 {
        let deployment = ModelServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration("fixture-artifact", 32_768)),
                    },
                ),
                    ),
                exposure: Some(support::hidden_model_exposure()),
                        model_deployment_name: None,
            }),
        )
        .await
        .expect("load")
        .into_inner()
        .deployment
        .expect("loaded deployment");
        wait_for_lifecycle(
            &service,
            &deployment.model_deployment_id,
            ModelDeploymentStatus::Ready,
        )
        .await;

        ModelServiceContract::remove_model_deployment(
            &service,
            Request::new(RemoveModelDeploymentRequest {
                model_deployment_id: deployment.model_deployment_id,
            }),
        )
        .await
        .expect("unload");
        wait_for_deployment_removal(&service).await;
    }

    drop(service);
}

#[tokio::test]
async fn concurrent_load_cannot_reverse_serialized_unload_intent() {
    let root = support::temporary_fast_fixture();
    let site_path = root.join("site.yaml");
    let source = fs::read_to_string(&site_path).expect("desired site fixture");
    fs::write(
        &site_path,
        source.replace(
            FAST_TEST_SCENARIO,
            "defaultScenario:\n        readyDelayMillis: 0\n        stopDelayMillis: 1000",
        ),
    )
    .expect("stop-delay fixture");
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");
    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;

    let (unload, load) = tokio::join!(
        ModelServiceContract::remove_model_deployment(
            &service,
            Request::new(RemoveModelDeploymentRequest {
                model_deployment_id: deployment.model_deployment_id.clone(),
            }),
        ),
        ModelServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration("fixture-artifact", 32_768)),
                    },
                ),
                    ),
                exposure: Some(support::hidden_model_exposure()),
                        model_deployment_name: None,
            }),
        ),
    );
    assert!(unload.is_ok());
    let loaded = load
        .expect("independent model deployment remains accepted")
        .into_inner()
        .deployment
        .expect("second deployment");
    assert_ne!(loaded.model_deployment_id, deployment.model_deployment_id);
    let retained: Site = dotcore_proto::yaml::from_file(&site_path).expect("retained desired site");
    assert_eq!(retained.model_deployments.len(), 2);
    let first = retained
        .model_deployments
        .iter()
        .find(|item| item.model_deployment_id == deployment.model_deployment_id)
        .expect("first deployment");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(first.desired_state),
        Ok(ModelDeploymentDesiredState::Teardown)
    );
    let second = retained
        .model_deployments
        .iter()
        .find(|item| item.model_deployment_id == loaded.model_deployment_id)
        .expect("second deployment");
    assert_eq!(
        ModelDeploymentDesiredState::try_from(second.desired_state),
        Ok(ModelDeploymentDesiredState::Present)
    );
    drop(service);
}

#[tokio::test]
async fn fixture_scenario_keeps_lifecycle_transitions_past_one_console_poll_interval() {
    // Shorter transitions can complete between two console polls and never be displayed.
    const CONSOLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

    let root = support::temporary_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");
    service.start_observer();

    // Measured from before each request, so scheduling delays can only lengthen the observed
    // transition and never fail the lower bound.
    let loading_requested = Instant::now();
    let deployment = ModelServiceContract::create_model_deployment(
        &service,
        Request::new(CreateModelDeploymentRequest {
            source: Some(
                dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration(
                            "fixture-artifact",
                            32_768,
                        )),
                    },
                ),
            ),
            exposure: Some(support::hidden_model_exposure()),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load")
    .into_inner()
    .deployment
    .expect("loaded deployment");

    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Loading,
    )
    .await;
    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Ready,
    )
    .await;
    let loading_duration = loading_requested.elapsed();
    assert!(
        loading_duration >= CONSOLE_POLL_INTERVAL,
        "loading became ready after {loading_duration:?}"
    );

    let unloading_requested = Instant::now();
    ModelServiceContract::remove_model_deployment(
        &service,
        Request::new(RemoveModelDeploymentRequest {
            model_deployment_id: deployment.model_deployment_id.clone(),
        }),
    )
    .await
    .expect("unload");
    wait_for_lifecycle(
        &service,
        &deployment.model_deployment_id,
        ModelDeploymentStatus::Unloading,
    )
    .await;
    wait_for_deployment_removal(&service).await;
    let unloading_duration = unloading_requested.elapsed();
    assert!(
        unloading_duration >= CONSOLE_POLL_INTERVAL,
        "unloading completed after {unloading_duration:?}"
    );

    drop(service);
}

async fn wait_for_lifecycle(
    service: &impl ModelServiceContract,
    deployment_id: &str,
    lifecycle: ModelDeploymentStatus,
) {
    let mut last_state = None;
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            last_state = ModelServiceContract::list_model_deployments(
                service,
                Request::new(ListModelDeploymentsRequest {}),
            )
            .await
            .expect("deployment read")
            .into_inner()
            .deployments
            .into_iter()
            .find(|deployment| deployment.model_deployment_id == deployment_id)
            .and_then(|deployment| deployment.state);
            if last_state
                .as_ref()
                .is_some_and(|state| state.status == lifecycle as i32)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("expected lifecycle state {lifecycle:?}; last state: {last_state:?}")
    });
}

async fn wait_for_deployment_removal(service: &impl ModelServiceContract) {
    let mut remaining = Vec::new();
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            remaining = ModelServiceContract::list_model_deployments(
                service,
                Request::new(ListModelDeploymentsRequest {}),
            )
            .await
            .expect("deployment read")
            .into_inner()
            .deployments;
            if remaining.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("deployment removal; last deployments: {remaining:?}"));
}

#[tokio::test]
async fn concurrent_load_and_deployment_read_never_return_a_partial_deployment() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let service = model_service_for(&configuration)
        .await
        .expect("Model service");

    let (loaded, listed) = tokio::join!(
        ModelServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Local(
                    dotcore_proto::dotcore::model::v1::CreateLocalModelDeploymentRequest {
                        model_id: "fixture-model".to_owned(),
                        profile_id: "fixture-profile".to_owned(),
                        placement: Some(support::exact_placement("fixture-node", "fixture-target")),
                        configuration: Some(support::local_model_configuration("fixture-artifact", 32_768)),
                    },
                ),
                    ),
                exposure: Some(support::hidden_model_exposure()),
                        model_deployment_name: None,
            }),
        ),
        ModelServiceContract::list_model_deployments(
            &service,
            Request::new(ListModelDeploymentsRequest {}),
        ),
    );

    loaded.expect("Load must succeed");
    let listed = listed
        .expect("List must not observe a partial mutation")
        .into_inner();
    assert!(listed.deployments.iter().all(|deployment| {
        deployment.snapshot.is_some() && deployment.state.is_some() && deployment.intent.is_some()
    }));
    drop(service);
}

fn application_lifecycle_configuration(root: &Path) -> dotcore_domain::SitePaths {
    let catalog = root.join("application-lifecycle-catalog");
    copy_catalog(&support::simulated_master_catalog(), &catalog);
    dotcore_domain::SitePaths::from_values(
        Some(root.to_str().expect("fixture site root")),
        Some(catalog.to_str().expect("fixture catalog root")),
        Some(root.join("cache").to_str().expect("fixture artifact cache")),
    )
    .expect("application lifecycle fixture configuration")
}

#[test]
fn copied_catalog_declarations_are_writable() {
    let root = support::temporary_fixture();
    let destination = root.join("catalog-copy");
    copy_catalog(&support::workspace_root().join("catalog"), &destination);
    let application_path = destination.join("applications/open-webui.yaml");
    let application = fs::read_to_string(&application_path).expect("copied catalog declaration");

    fs::write(&application_path, &application).expect("rewrite copied catalog declaration");
    assert_eq!(
        fs::read_to_string(application_path).expect("read rewritten catalog declaration"),
        application,
    );
}

#[test]
fn copied_read_only_catalog_preserves_bytes_and_provides_a_writable_private_copy() {
    let root = support::temporary_fixture();
    let source = root.join("read-only-catalog");
    copy_catalog(&support::workspace_root().join("catalog"), &source);
    let read_only_source = ReadOnlyCatalog::new(&source);
    let destination = root.join("catalog-copy");

    copy_catalog(&source, &destination);

    assert_catalog_bytes_and_structure_match(&source, &destination);
    assert_catalog_copy_is_private_and_writable(&destination);
    let application_path = destination.join("applications/open-webui.yaml");
    let application = fs::read(&application_path).expect("copied catalog declaration");
    fs::write(&application_path, &application).expect("rewrite copied catalog declaration");
    assert_eq!(
        fs::read(&application_path).expect("read rewritten catalog declaration"),
        application,
    );
    drop(read_only_source);
}

fn copy_catalog(source: &std::path::Path, destination: &std::path::Path) {
    fs::create_dir_all(destination).expect("fixture catalog root");
    set_private_catalog_directory(destination);
    for directory in ["applications", "engines", "hardware-classes", "models"] {
        copy_catalog_directory(&source.join(directory), &destination.join(directory));
    }
}

fn copy_catalog_directory(source: &std::path::Path, destination: &std::path::Path) {
    fs::create_dir_all(destination).expect("fixture catalog directory");
    set_private_catalog_directory(destination);
    for entry in fs::read_dir(source).expect("Catalog source directory") {
        let entry = entry.expect("Catalog source entry");
        let destination_entry = destination.join(entry.file_name());
        if entry
            .file_type()
            .expect("Catalog source entry type")
            .is_dir()
        {
            copy_catalog_directory(&entry.path(), &destination_entry);
        } else {
            fs::copy(entry.path(), &destination_entry).expect("copy catalog declaration");
            set_private_catalog_file(&destination_entry);
        }
    }
}

fn set_private_catalog_directory(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .expect("private fixture catalog directory");
}

fn set_private_catalog_file(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .expect("private fixture catalog declaration");
}

struct ReadOnlyCatalog {
    entries: Vec<(PathBuf, bool)>,
}

impl ReadOnlyCatalog {
    fn new(root: &Path) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let entries = catalog_entries(root);
        for (path, is_directory) in &entries {
            if !is_directory {
                fs::set_permissions(path, fs::Permissions::from_mode(0o400))
                    .expect("read-only catalog file");
            }
        }
        for (path, is_directory) in entries.iter().rev() {
            if *is_directory {
                fs::set_permissions(path, fs::Permissions::from_mode(0o500))
                    .expect("read-only catalog directory");
            }
        }
        Self { entries }
    }
}

impl Drop for ReadOnlyCatalog {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;

        for (path, is_directory) in &self.entries {
            if *is_directory {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
            }
        }
        for (path, is_directory) in &self.entries {
            if !is_directory {
                let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
            }
        }
    }
}

fn assert_catalog_bytes_and_structure_match(source: &Path, destination: &Path) {
    let source_entries = catalog_entries(source);
    let destination_entries = catalog_entries(destination);
    assert_eq!(
        source_entries
            .iter()
            .map(|(path, is_directory)| (
                path.strip_prefix(source).expect("source entry"),
                is_directory
            ))
            .collect::<Vec<_>>(),
        destination_entries
            .iter()
            .map(|(path, is_directory)| (
                path.strip_prefix(destination).expect("destination entry"),
                is_directory
            ))
            .collect::<Vec<_>>(),
        "Catalog copy structure",
    );
    for (source_path, is_directory) in source_entries {
        if !is_directory {
            let relative_path = source_path.strip_prefix(source).expect("source file");
            assert_eq!(
                fs::read(&source_path).expect("source catalog declaration"),
                fs::read(destination.join(relative_path)).expect("copied catalog declaration"),
                "Catalog declaration bytes for {}",
                relative_path.display(),
            );
        }
    }
}

fn assert_catalog_copy_is_private_and_writable(root: &Path) {
    use std::os::unix::fs::MetadataExt;

    for (path, is_directory) in catalog_entries(root) {
        assert_eq!(
            fs::metadata(&path)
                .expect("copied catalog entry metadata")
                .mode()
                & 0o777,
            if is_directory { 0o700 } else { 0o600 },
            "private writable mode for {}",
            path.display(),
        );
    }
}

fn catalog_entries(root: &Path) -> Vec<(PathBuf, bool)> {
    fn visit(path: &Path, entries: &mut Vec<(PathBuf, bool)>) {
        let is_directory = path.is_dir();
        entries.push((path.to_owned(), is_directory));
        if is_directory {
            let mut children = fs::read_dir(path)
                .expect("Catalog directory")
                .collect::<Result<Vec<_>, _>>()
                .expect("Catalog directory entry");
            children.sort_by_key(|entry| entry.file_name());
            for child in children {
                visit(&child.path(), entries);
            }
        }
    }

    let mut entries = Vec::new();
    visit(root, &mut entries);
    entries
}

#[tokio::test]
async fn profile_compatibility_sizes_a_scenario_against_the_simulated_target_memory() {
    let root = support::temporary_fast_fixture();
    let model_path = root.join("catalog/models/fixture-model.yaml");
    let model = fs::read_to_string(&model_path).expect("model fixture");
    let support = "    hardwareSupport:\n      - hardwareClassId: \"fixture-nvidia\"\n        hardwareRequirements:\n          minimumSystemMemoryBytes: \"1\"\n";
    let sized = format!(
        "    sizing:\n      parameters: {{ baseBytes: 20000000000, bytesPerContextToken: 100000, slotStateBytes: 500000000 }}\n      estimates:\n        acceleratorMemory:\n          unit: bytes\n          equation: \"${{baseBytes}} + ${{bytesPerContextToken}} * ${{total-context}} + ${{parallel-requests}} * ${{slotStateBytes}}\"\n{support}        bindingSizing: [{{ bindingId: \"fixture-artifact\" }}]\n"
    );
    assert!(model.contains(support), "fixture hardware support");
    fs::write(&model_path, model.replace(support, &sized)).expect("sized model fixture");
    let service = model_service_for(&support::configuration(&root))
        .await
        .expect("model service");
    let read = |total_context_tokens, context_tokens_per_request| {
        ModelServiceContract::list_model_profile_compatibility(
            &service,
            Request::new(ListModelProfileCompatibilityRequest {
                model_id: Some("fixture-model".to_owned()),
                target: None,
                scenario: Some(SizingScenario {
                    total_context_tokens,
                    context_tokens_per_request,
                }),
            }),
        )
    };

    // The manager's own node reports its first observation asynchronously after composition.
    let deadline = Instant::now() + Duration::from_secs(10);
    let fits = loop {
        let response = read(32_768, Some(8192))
            .await
            .expect("compatibility read")
            .into_inner();
        if !response.entries.is_empty() || Instant::now() >= deadline {
            break response;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let exceeds = read(131_072, None)
        .await
        .expect("compatibility read")
        .into_inner();

    // The fixture node declares an RTX 5090 desktop: 34190917632 bytes of dedicated memory with
    // 521142272 held by the driver, so 33669775360 bytes free less a 1% headroom of the total
    // (341909176) leaves 33327866184 usable. A total context of 32768 tokens at the engine's 4
    // parallel requests needs 20000000000 + 100000 × 32768 + 4 × 500000000 bytes; 131072 tokens
    // need 20000000000 + 100000 × 131072 + 4 × 500000000 bytes, and the omitted context per
    // request fills to the total context. The largest fit is ⌊(33327866184 − 22000000000) / 100000⌋.
    for (
        response,
        total_context,
        context_per_request,
        accelerator_memory_bytes,
        memory_fit,
        free_memory_fit,
    ) in [
        (
            fits,
            32_768,
            8192,
            25_276_800_000,
            MemoryFit::Fits,
            MemoryFit::Fits,
        ),
        (
            exceeds,
            131_072,
            131_072,
            35_107_200_000,
            MemoryFit::Exceeds,
            MemoryFit::Exceeds,
        ),
    ] {
        assert_eq!(
            response
                .profiles
                .iter()
                .map(|profile| (
                    profile.profile_id.as_str(),
                    profile.engine_parameter_values.clone(),
                    profile
                        .hardware_class_estimates
                        .iter()
                        .map(|class| (
                            class.hardware_class_id.as_str(),
                            class
                                .estimates
                                .iter()
                                .map(|estimate| (estimate.resource.as_str(), estimate.value))
                                .collect::<Vec<_>>()
                        ))
                        .collect::<Vec<_>>(),
                    profile.unsupported_reason
                ))
                .collect::<Vec<_>>(),
            [(
                "fixture-profile",
                Some(EngineParameterValues {
                    values: [
                        ("prompt-cache-memory-mib", 0),
                        ("total-context", total_context),
                        ("parallel-requests", 4),
                        ("context-per-request", context_per_request),
                    ]
                    .into_iter()
                    .map(|(parameter_id, value)| EngineParameterValue {
                        parameter_id: parameter_id.to_owned(),
                        value: Some(engine_parameter_value::Value::UnsignedInteger(value)),
                    })
                    .collect(),
                }),
                vec![(
                    "fixture-nvidia",
                    vec![("acceleratorMemory", accelerator_memory_bytes)]
                )],
                SizingScenarioUnsupportedReason::Unspecified as i32
            )]
        );
        assert_eq!(
            response
                .entries
                .iter()
                .map(|entry| (
                    entry.target.clone(),
                    entry.status,
                    entry.hardware_class_id.as_str(),
                    entry.memory_fit,
                    entry.free_memory_fit,
                    entry.unsupported_reason,
                    entry.largest_total_context_tokens
                ))
                .collect::<Vec<_>>(),
            [(
                Some(support::exact_placement("fixture-node", "fixture-target")),
                ModelCompatibilityStatus::Runs as i32,
                "fixture-nvidia",
                memory_fit as i32,
                free_memory_fit as i32,
                SizingScenarioUnsupportedReason::Unspecified as i32,
                Some(113_278)
            )]
        );
    }

    for (total_context, context_per_request, message) in [
        (
            1023,
            None,
            "Sizing scenario requires a total context of at least 1024 tokens",
        ),
        (
            8192,
            Some(8193),
            "Sizing scenario context per request must not be above its total context",
        ),
    ] {
        let invalid = read(total_context, context_per_request)
            .await
            .expect_err("invalid scenario");
        assert_eq!(invalid.code(), tonic::Code::InvalidArgument);
        assert_eq!(invalid.message(), message);
    }
}

#[tokio::test]
async fn profile_compatibility_reads_the_simulated_site_through_the_model_service() {
    let root = support::temporary_fast_fixture();
    let service = model_service_for(&support::configuration(&root))
        .await
        .expect("model service");
    let read = |target| {
        ModelServiceContract::list_model_profile_compatibility(
            &service,
            Request::new(ListModelProfileCompatibilityRequest {
                model_id: Some("fixture-model".to_owned()),
                target,
                scenario: None,
            }),
        )
    };

    // The manager's own node reports its first observation asynchronously after composition.
    let deadline = Instant::now() + Duration::from_secs(10);
    let response = loop {
        let response = read(None).await.expect("compatibility read").into_inner();
        if response
            .targets
            .iter()
            .all(|target| target.status != ModelCompatibilityStatus::Unknown as i32)
            || Instant::now() >= deadline
        {
            break response;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    assert_eq!(
        response
            .targets
            .iter()
            .map(|target| (target.target.clone(), target.status, target.reason.clone()))
            .collect::<Vec<_>>(),
        [(
            Some(support::exact_placement("fixture-node", "fixture-target")),
            ModelCompatibilityStatus::Runs as i32,
            None
        )]
    );
    assert_eq!(
        response
            .entries
            .iter()
            .map(|entry| (
                entry.model_id.as_str(),
                entry.profile_id.as_str(),
                entry.target.clone(),
                entry.status,
                entry.advisories.clone()
            ))
            .collect::<Vec<_>>(),
        [(
            "fixture-model",
            "fixture-profile",
            Some(support::exact_placement("fixture-node", "fixture-target")),
            ModelCompatibilityStatus::Runs as i32,
            Vec::new()
        )]
    );

    let application_target = read(Some(support::exact_placement(
        "fixture-node",
        "fixture-application-target",
    )))
    .await
    .expect_err("application target");
    assert_eq!(application_target.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        application_target.message(),
        "Site execution target \"fixture-application-target\" is not a model execution target"
    );
}

/// Master catalog copy whose only recommendations are the synthetic ones the recommendation read
/// test expects: coding lists the fixture profile on the fixture class, and decision making lists
/// nothing.
fn recommendation_master_catalog(root: &Path) -> PathBuf {
    let master = root.join("cache/recommendation-master-catalog");
    support::copy_directory(&support::simulated_master_catalog(), &master);
    let recommendations = master.join("recommendations");
    fs::remove_dir_all(&recommendations).expect("master recommendations");
    dotcore_catalog::test_support::write_recommendations(
        &master,
        "fixture-nvidia",
        r#"hardwareClassId: "fixture-nvidia"
useCases:
  - useCase: "USE_CASE_CODING_AND_AGENTS"
    entries:
      - modelId: "fixture-model"
        profileId: "fixture-profile"
"#,
    );
    master
}

#[tokio::test]
async fn model_recommendations_rank_the_simulated_site_through_the_model_service() {
    let root = support::temporary_fast_fixture();
    // A sized model on a declared machine gives the fit a memory estimate and observed memory.
    support::declare_fixture_machine(&root, &["rtx-5090"]);
    fs::write(
        root.join("catalog/models/fixture-model.yaml"),
        support::SIZED_FIXTURE_MODEL,
    )
    .expect("sized fixture model");
    let mut paths = support::configuration(&root);
    paths.master_catalog_root = Some(recommendation_master_catalog(&root));
    let service = model_service_for(&paths).await.expect("model service");
    let read = |use_cases: Vec<UseCase>| {
        ModelServiceContract::list_model_recommendations(
            &service,
            Request::new(ListModelRecommendationsRequest {
                use_cases: use_cases
                    .into_iter()
                    .map(|use_case| use_case as i32)
                    .collect(),
            }),
        )
    };

    // The manager's own node reports its first observation asynchronously after composition.
    let deadline = Instant::now() + Duration::from_secs(10);
    let response = loop {
        let response = read(vec![UseCase::CodingAndAgents])
            .await
            .expect("recommendation read")
            .into_inner();
        if response
            .use_cases
            .iter()
            .any(|ranked| !ranked.rows.is_empty())
            || Instant::now() >= deadline
        {
            break response;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    // The starting context is the one the compatibility read gives the same profile.
    let compatibility = ModelServiceContract::list_model_profile_compatibility(
        &service,
        Request::new(ListModelProfileCompatibilityRequest {
            model_id: Some("fixture-model".to_owned()),
            target: None,
            scenario: None,
        }),
    )
    .await
    .expect("compatibility read")
    .into_inner();
    let [compatible] = compatibility.entries.as_slice() else {
        panic!("one compatibility entry: {compatibility:?}");
    };
    let starting_total_context_tokens = compatible
        .starting_total_context_tokens
        .map(u64::from)
        .expect("a starting total context");
    assert_eq!(
        response.use_cases,
        [UseCaseModelRecommendations {
            use_case: UseCase::CodingAndAgents as i32,
            rows: vec![RecommendedModelProfile {
                model_id: "fixture-model".to_owned(),
                profile_id: "fixture-profile".to_owned(),
                targets: vec![RecommendedProfileTarget {
                    target: Some(support::exact_placement("fixture-node", "fixture-target")),
                    fit: ModelRecommendationFit::FitsNow as i32,
                    deployed: false,
                    starting_total_context_tokens: Some(starting_total_context_tokens),
                }],
            }],
        }]
    );

    // No use case asks for all four, each ranked even when nothing is recommended.
    let all = read(Vec::new()).await.expect("all use cases").into_inner();
    assert_eq!(
        all.use_cases
            .iter()
            .map(|ranked| (ranked.use_case, ranked.rows.len()))
            .collect::<Vec<_>>(),
        [
            (UseCase::CodingAndAgents as i32, 1),
            (UseCase::GeneralAssistant as i32, 0),
            (UseCase::Translation as i32, 0),
            (UseCase::DecisionMaking as i32, 0),
        ]
    );

    let unspecified = read(vec![UseCase::CodingAndAgents, UseCase::Unspecified])
        .await
        .expect_err("unspecified use case");
    assert_eq!(unspecified.code(), tonic::Code::InvalidArgument);
    assert_eq!(
        unspecified.message(),
        "Model recommendations require specified use cases"
    );
}

#[tokio::test]
async fn rejected_local_configurations_return_their_reason_through_the_model_service() {
    let root = support::temporary_fast_fixture();
    let model_path = root.join("catalog/models/fixture-model.yaml");
    let model = fs::read_to_string(&model_path).expect("model fixture");
    let single_micro_batch = model
        .replacen(
            "  - configurationId: \"fixture-configuration\"\n",
            "  - configurationId: \"fixture-configuration\"\n    maximumContextTokens: 16384\n",
            1,
        )
        .replacen(
            "fixedSettings: { gpuLayers: 0 }",
            "fixedSettings: { gpuLayers: 0, singleMicroBatch: true }",
            1,
        );
    assert_ne!(single_micro_batch, model, "single micro-batch fixture");
    fs::write(&model_path, single_micro_batch).expect("single micro-batch model");
    let service = model_service_for(&support::configuration(&root))
        .await
        .expect("model service");
    let configuration = |values: &[(&str, u64)]| {
        let mut configuration = support::local_model_configuration("fixture-artifact", 16_384);
        let parameters = &mut configuration
            .engine_parameter_values
            .as_mut()
            .expect("engine parameter values")
            .values;
        for (parameter_id, value) in values {
            parameters.retain(|parameter| parameter.parameter_id != *parameter_id);
            parameters.push(EngineParameterValue {
                parameter_id: (*parameter_id).to_owned(),
                value: Some(engine_parameter_value::Value::UnsignedInteger(*value)),
            });
        }
        CreateLocalModelDeploymentRequest {
            model_id: "fixture-model".to_owned(),
            profile_id: "fixture-profile".to_owned(),
            placement: Some(support::exact_placement("fixture-node", "fixture-target")),
            configuration: Some(configuration),
        }
    };

    for (values, message) in [
        (
            [("total-context", 16_385)].as_slice(),
            "invalid engine parameter values: total context of 16385 tokens is above the maximum context of 16384 tokens of configuration \"fixture-configuration\", which processes prompts in a single llama.cpp micro-batch; lower the total context",
        ),
        (
            [("context-per-request", 16_385)].as_slice(),
            "engine parameter values give each request 16385 tokens of context, above the maximum context of 16384 tokens of configuration \"fixture-configuration\" of catalog model \"fixture-model\"; reduce the context per request",
        ),
        (
            [("parallel-requests", 65)].as_slice(),
            "invalid engine parameter values: profile \"fixture-profile\" engine parameter \"parallel-requests\" value 65 is outside its range 1..=64",
        ),
    ] {
        let preview = ModelServiceContract::preview_local_model_deployment(
            &service,
            Request::new(PreviewLocalModelDeploymentRequest {
                local: Some(configuration(values)),
            }),
        )
        .await
        .expect_err("rejected preview");
        assert_eq!(
            (preview.code(), preview.message()),
            (tonic::Code::InvalidArgument, message)
        );

        let created = ModelServiceContract::create_model_deployment(
            &service,
            Request::new(CreateModelDeploymentRequest {
                source: Some(create_model_deployment_request::Source::Local(
                    configuration(values),
                )),
                exposure: Some(support::hidden_model_exposure()),
                model_deployment_name: None,
            }),
        )
        .await
        .expect_err("rejected creation");
        assert_eq!(
            (created.code(), created.message()),
            (tonic::Code::InvalidArgument, message)
        );
    }
    let site: Site =
        dotcore_proto::yaml::from_file(&root.join("site.yaml")).expect("site after rejections");
    assert_eq!(
        (site.model_deployments, site.inference_routes),
        (Vec::new(), Vec::new())
    );

    ModelServiceContract::preview_local_model_deployment(
        &service,
        Request::new(PreviewLocalModelDeploymentRequest {
            local: Some(configuration(&[])),
        }),
    )
    .await
    .expect("the maximum total context");
}
