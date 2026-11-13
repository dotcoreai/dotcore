//! Development gRPC-Web transport integration tests.

use std::{
    error::Error as _,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_openai::{
    Client,
    config::OpenAIConfig,
    types::chat::{
        ChatCompletionMessageToolCall, ChatCompletionMessageToolCalls,
        ChatCompletionRequestAssistantMessageArgs, ChatCompletionRequestMessage,
        ChatCompletionRequestToolMessageArgs, ChatCompletionRequestToolMessageContent,
        ChatCompletionRequestUserMessageArgs, ChatCompletionRequestUserMessageContent,
        ChatCompletionStreamOptions, ChatCompletionTool, ChatCompletionTools,
        CreateChatCompletionRequestArgs, FunctionCall, FunctionObject,
    },
};
use dotcore::composition::CompositionError;
use dotcore::composition::model_service_for;
use dotcore::testing::writer_metric_descriptors;
use dotcore_domain::{
    clock::{Clock, ClockSleep},
    hardware_observation::{HardwareSample, HardwareSnapshotSource},
};
use dotcore_node_hardware::hardware::{
    AcceleratorDiscovery, AcceleratorVendorDiscoverer, AcceleratorVendorObservation,
    AcceleratorVendorPoller, HardwareSnapshotCollector, HostFactsAdapter, UnavailableReason,
};
use dotcore_observability::validate_catalog;
use dotcore_proto::{
    dotcore::{
        catalog::v1::{ListModelsRequest, catalog_service_client::CatalogServiceClient},
        diagnostic::v1::ReadBackendDiagnosticsResponse,
        metrics::v1::{
            KeyedMetricQuery, ListMetricDescriptorsRequest, ListMetricDescriptorsResponse,
            MetricAggregate, MetricDescriptor, MetricDimension, MetricDistributionValue,
            MetricKind, MetricPercentile, MetricPoint, MetricProvenance, MetricQuery, MetricReduce,
            MetricUnit, QueryMetricsRequest, QueryMetricsResponse, WatchMetricsRequest,
            WatchMetricsResponse, metric_point, metric_query, watch_metrics_response,
        },
        model::v1::{
            CreateModelDeploymentRequest, DirectRouteExposure, ModelDeploymentDesiredState,
            ModelDeploymentExposure, PauseModelRequest, model_deployment_exposure,
            model_deployment_service_client::ModelDeploymentServiceClient,
            model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
        },
        observation::v1::{
            AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, AcceleratorVendor,
            HardwareSnapshot, HostFacts, HostObservation, ObservationUnavailable, RuntimeKind,
            RuntimeObservation, accelerator_observation, host_observation, runtime_observation,
        },
        site::v1::{
            ConsoleSettings, CurrentHardwareSnapshotAvailability, GetConsoleSettingsResponse,
            HardwareRefreshState, ListHardwareSnapshotsResponse, ListMachinesResponse, Machine,
            MachineHardwareSnapshot, NodeObservationFreshness, NodeObservationState, Site,
            SiteNode, UpdateConsoleSettingsRequest, current_hardware_snapshot_availability,
            hardware_refresh_state, site_service_client::SiteServiceClient,
        },
        watch::v1::{
            KeyedResource, ResourceSubscription, WatchResourcesRequest, WatchResourcesResponse,
            keyed_resource::Payload, resource_subscription, watch_resources_response,
        },
    },
    google::protobuf::{Empty, FieldMask, Timestamp},
};
use dotcore_runtime_backends::simulated::{
    SimulatedRuntimeCallCounts, SimulatedRuntimeTestSupport,
};
use dotcore_site::repository::SiteRepositoryError;
use prost::Message;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Notify, oneshot},
};
use tokio_stream::StreamExt;

use crate::support;

#[tokio::test]
async fn only_manager_exposes_management_and_inference_apis() {
    let Some(harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_endpoint = tonic::transport::Endpoint::from_shared(format!(
        "http://{}",
        harness
            .manager()
            .addresses()
            .management
            .expect("management listener")
    ))
    .expect("manager management endpoint");

    let mut manager_catalog = CatalogServiceClient::connect(manager_endpoint.uri().to_owned())
        .await
        .expect("manager management API connection");
    let manager_models = manager_catalog
        .list_models(ListModelsRequest {})
        .await
        .expect("manager catalog API availability")
        .into_inner();
    assert!(
        manager_models
            .models
            .iter()
            .any(|model| model.model_id == "fixture-model")
    );

    let http = reqwest::Client::new();
    let manager_inference = http
        .get(format!(
            "http://{}/v1/models",
            harness
                .manager()
                .addresses()
                .inference
                .expect("inference listener")
        ))
        .send()
        .await
        .expect("manager inference API connection");
    assert_eq!(manager_inference.status(), reqwest::StatusCode::OK);
    assert_eq!(
        manager_inference
            .json::<serde_json::Value>()
            .await
            .expect("manager inference response"),
        serde_json::json!({"object": "list", "data": []})
    );

    let worker_addresses = harness.worker().addresses();
    assert_eq!(worker_addresses.management, None);
    assert_eq!(worker_addresses.inference, None);
}

#[tokio::test]
async fn two_process_harness_isolates_processes_ports_state_and_cleanup() {
    let Some(mut harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let manager_addresses = harness.manager().addresses();
    let worker_addresses = harness.worker().addresses();

    assert_ne!(harness.manager().site_root(), harness.worker().site_root());
    assert!(harness.manager().site_root().join("site.yaml").is_file());
    assert!(!harness.worker().site_root().join("site.yaml").exists());
    assert!(!harness.worker().node_binding_path().exists());
    assert!(harness.worker().site_root().join("worker.yaml").is_file());
    assert_ne!(
        harness.manager().state_path(),
        harness.worker().node_state_path()
    );
    assert_ne!(
        harness.manager().site_root().join("cache"),
        harness.worker().site_root().join("cache")
    );
    assert_eq!(
        [
            manager_addresses.management.expect("management listener"),
            manager_addresses.inference.expect("inference listener"),
            manager_addresses.private.expect("manager private listener"),
            worker_addresses.private.expect("worker private listener"),
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>()
        .len(),
        4
    );
    assert_eq!(worker_addresses.management, None);
    assert_eq!(worker_addresses.inference, None);
    assert!(harness.manager_mut().is_running());
    assert!(harness.worker_mut().is_running());
    tokio::net::TcpStream::connect(manager_addresses.management.expect("management listener"))
        .await
        .expect("manager management listener");
    support::wait_for_node_realizer_bootstrap(harness.worker().node_state_path()).await;
    assert!(
        !harness.manager().state_path().exists(),
        "startup writes no owner state for a site without deployments"
    );
    assert!(harness.worker().node_state_path().is_file());
    assert!(
        !harness.worker().state_path().exists(),
        "a worker keeps its node state only in its node store"
    );
    assert_eq!(
        std::fs::read_to_string(harness.manager().node_binding_path())
            .expect("manager node binding"),
        std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../fixtures/simulated-site/node.yaml")
        )
        .expect("fixture node binding"),
        "the manager keeps the fixture's node binding"
    );

    let original_worker_addresses = harness.worker().addresses();
    harness.manager_mut().stop();
    assert!(!harness.manager_mut().is_running());
    assert!(harness.worker_mut().is_running());
    assert_eq!(harness.worker().addresses(), original_worker_addresses);
    harness.manager_mut().restart();
    assert!(harness.manager_mut().is_running());
    assert!(harness.worker_mut().is_running());

    let restarted_manager_addresses = harness.manager().addresses();
    harness.worker_mut().stop();
    assert!(harness.manager_mut().is_running());
    assert!(!harness.worker_mut().is_running());
    assert_eq!(harness.manager().addresses(), restarted_manager_addresses);
    harness.worker_mut().restart();
    assert!(harness.manager_mut().is_running());
    assert!(harness.worker_mut().is_running());

    let manager_root = harness.manager().site_root().to_owned();
    let worker_root = harness.worker().site_root().to_owned();
    let manager_site_before = std::fs::read(manager_root.join("site.yaml")).expect("manager site");
    let worker_manifest_before =
        std::fs::read(worker_root.join("worker.yaml")).expect("node folder");
    let manager_node_before =
        std::fs::read(harness.manager().node_binding_path()).expect("manager node binding");
    let worker_state_before =
        std::fs::read(harness.worker().node_state_path()).expect("worker state");
    harness.manager_mut().restart();
    assert_eq!(
        std::fs::read(manager_root.join("site.yaml")).expect("manager site after restart"),
        manager_site_before
    );
    assert_eq!(
        std::fs::read(harness.manager().node_binding_path())
            .expect("manager binding after restart"),
        manager_node_before
    );
    assert_eq!(
        std::fs::read(worker_root.join("worker.yaml")).expect("node folder after manager restart"),
        worker_manifest_before
    );
    assert_eq!(
        std::fs::read(harness.worker().node_state_path())
            .expect("worker state after manager restart"),
        worker_state_before
    );
    // Startup writes no owner state for this deployment-free site, so it may stay absent.
    let manager_state_after_restart = std::fs::read(harness.manager().state_path()).ok();

    harness.worker_mut().restart();
    assert_eq!(
        std::fs::read(worker_root.join("worker.yaml")).expect("node folder after restart"),
        worker_manifest_before
    );
    assert_eq!(
        std::fs::read(manager_root.join("site.yaml")).expect("manager site after worker restart"),
        manager_site_before
    );
    assert_eq!(
        std::fs::read(harness.manager().node_binding_path())
            .expect("manager binding after worker restart"),
        manager_node_before
    );
    assert_eq!(
        std::fs::read(harness.manager().state_path()).ok(),
        manager_state_after_restart
    );
    let manager_lock = std::path::PathBuf::from(format!("{}.lock", manager_root.display()));
    let worker_lock = std::path::PathBuf::from(format!("{}.lock", worker_root.display()));
    drop(harness);
    assert!(!manager_root.exists());
    assert!(!worker_root.exists());
    assert!(!manager_lock.exists());
    assert!(!worker_lock.exists());

    let Some(panic_harness) = support::TwoProcessObservationHarness::start() else {
        return;
    };
    let panic_manager_root = panic_harness.manager().site_root().to_owned();
    let panic_worker_root = panic_harness.worker().site_root().to_owned();
    let panic_manager_lock =
        std::path::PathBuf::from(format!("{}.lock", panic_manager_root.display()));
    let panic_worker_lock =
        std::path::PathBuf::from(format!("{}.lock", panic_worker_root.display()));
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _harness = panic_harness;
        panic!("injected failed assertion");
    }));
    assert!(failure.is_err());
    assert!(!panic_manager_root.exists());
    assert!(!panic_worker_root.exists());
    assert!(!panic_manager_lock.exists());
    assert!(!panic_worker_lock.exists());
}

#[tokio::test]
async fn server_preserves_typed_composition_failures() {
    let root = support::temporary_fast_fixture();
    std::fs::remove_file(root.join("site.yaml")).expect("remove desired site fixture");

    let error =
        match support::prepare_backend(support::configuration(&root), std::future::pending()).await
        {
            Ok(_) => panic!("missing desired site must prevent preparation"),
            Err(error) => error,
        };

    let CompositionError::SiteRepository(composition) = &error else {
        panic!("server must preserve a SiteRepositoryError: {error}");
    };
    assert!(matches!(composition, SiteRepositoryError::Yaml(_)));
    assert!(composition.source().is_some());
}

#[tokio::test]
async fn backend_preparation_fails_when_the_inference_listener_cannot_bind() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let occupied_inference_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("occupied inference listener");
    let occupied_inference_address = occupied_inference_listener
        .local_addr()
        .expect("occupied inference address");

    let error = match dotcore::composition::Backend::prepare(
        dotcore::composition::BackendConfiguration {
            site_paths: configuration,
            telemetry_directory: None,
            dotcore_sources: Default::default(),
            role: dotcore::composition::BackendRole::Manager(
                dotcore::composition::ServerConfiguration {
                    management_bind_address: "127.0.0.1:0".parse().unwrap(),
                    inference_bind_address: occupied_inference_address.to_string().parse().unwrap(),
                    private_listener: dotcore::composition::ManagerPrivateListener::SiteDefault {
                        port: 0,
                    },
                    allowed_origins: vec!["http://127.0.0.1:5173".parse().unwrap()],
                },
            ),
        },
        std::future::pending(),
    )
    .await
    {
        Ok(_) => panic!("inference bind must fail"),
        Err(error) => error,
    };

    let CompositionError::Server(dotcore::server::ServerError::Listener(listener)) = &error else {
        panic!("typed inference listener error: {error:?}");
    };
    assert!(
        listener
            .to_string()
            .contains("failed to bind inference router")
    );
    assert!(
        listener
            .source()
            .expect("listener operating-system cause")
            .to_string()
            .contains("Address already in use")
    );
}

#[tokio::test]
async fn async_openai_client_runs_simulated_model_and_streamed_tool_chat() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let model_service = model_service_for(&configuration)
        .await
        .expect("Model service");
    model_service.start_observer();
    ModelServiceContract::create_model_deployment(
        &model_service,
        tonic::Request::new(CreateModelDeploymentRequest {
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
        }),
    )
    .await
    .expect("load fixture model");
    support::wait_for_model_ready(&model_service).await;
    drop(model_service);
    std::fs::remove_file(root.join("catalog/models/fixture-model.yaml"))
        .expect("remove catalog model after immutable deployment snapshot");
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend(configuration, async move {
        let _ = shutdown_signal.await;
    })
    .await
    .expect("prepared backend");
    let inference_address = backend.addresses().inference.expect("inference listener");
    let mut server = tokio::spawn(backend.serve());

    let client = Client::with_config(
        OpenAIConfig::new()
            .with_api_key("test")
            .with_api_base(format!("http://{inference_address}/v1")),
    );
    let models = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if server.is_finished() {
                let error = (&mut server)
                    .await
                    .expect("server task")
                    .expect_err("server must remain active");
                panic!("server failed before OpenAI connection: {error}");
            }
            match client.models().list().await {
                Ok(models) if models.data.iter().any(|model| model.id == "fixture-route") => {
                    return models;
                }
                Ok(_) => {}
                Err(_) => {}
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("inference listener readiness");
    assert!(models.data.iter().any(|model| model.id == "fixture-route"));

    let response = client
        .chat()
        .create(chat_request())
        .await
        .expect("synchronous Chat");
    assert_eq!(response.model, "fixture-route");
    assert_eq!(
        response.choices[0].message.content.as_deref(),
        Some("Simulated response.")
    );

    let mut stream = client
        .chat()
        .create_stream(tool_stream_request())
        .await
        .expect("streamed tool Chat");
    let mut tool_call_id = None;
    let mut function_name = None;
    let mut fragments = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.expect("stream chunk");
        for call in chunk
            .choices
            .into_iter()
            .flat_map(|choice| choice.delta.tool_calls.unwrap_or_default())
        {
            if let Some(id) = call.id {
                tool_call_id = Some(id);
            }
            if let Some(function) = call.function {
                if let Some(name) = function.name {
                    function_name = Some(name);
                }
                if let Some(arguments) = function.arguments {
                    fragments.push(arguments);
                }
            }
        }
    }
    let tool_call_id = tool_call_id.expect("streamed tool-call ID");
    let function_name = function_name.expect("streamed function name");
    let arguments = fragments.concat();
    assert_eq!(arguments, "{}");

    let follow_up = client
        .chat()
        .create(tool_result_request(tool_call_id, function_name, arguments))
        .await
        .expect("tool-result follow-up Chat");
    assert_eq!(follow_up.model, "fixture-route");
    assert_eq!(
        follow_up.choices[0].message.content.as_deref(),
        Some("Simulated response.")
    );

    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn manager_process_performs_no_runtime_operations_for_a_worker_target_deployment() {
    let root = support::temporary_fast_fixture();
    let configuration = support::configuration(&root);
    let model_service = model_service_for(&configuration)
        .await
        .expect("one-node model service");
    model_service.start_observer();
    ModelServiceContract::create_model_deployment(
        &model_service,
        tonic::Request::new(CreateModelDeploymentRequest {
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
                        route_name: "worker-route".to_owned(),
                    },
                )),
            }),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("one-node deployment lifecycle");
    drop(model_service);

    assign_fixture_target_to_worker(&root);
    let runtime = SimulatedRuntimeTestSupport::with_stop_failures(0);
    let backend = support::RunningBackend::start_with_simulated_runtime_test_support(
        configuration,
        runtime.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert_eq!(runtime.discovery_call_count(), 0);
    assert_eq!(runtime.call_counts(), SimulatedRuntimeCallCounts::default());

    backend.stop().await;
}

fn chat_request() -> async_openai::types::chat::CreateChatCompletionRequest {
    CreateChatCompletionRequestArgs::default()
        .model("fixture-route")
        .messages(vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content(ChatCompletionRequestUserMessageContent::Text(
                    "hello".to_owned(),
                ))
                .build()
                .expect("user message"),
        )])
        .build()
        .expect("Chat request")
}

fn tool_stream_request() -> async_openai::types::chat::CreateChatCompletionRequest {
    CreateChatCompletionRequestArgs::default()
        .model("fixture-route")
        .messages(vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessageArgs::default()
                .content(ChatCompletionRequestUserMessageContent::Text(
                    "use a tool".to_owned(),
                ))
                .build()
                .expect("user message"),
        )])
        .tools(vec![ChatCompletionTools::Function(ChatCompletionTool {
            function: FunctionObject {
                name: "read_file".to_owned(),
                description: None,
                parameters: Some(serde_json::json!({"type":"object"})),
                strict: None,
            },
        })])
        .stream_options(ChatCompletionStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        })
        .build()
        .expect("stream request")
}

fn tool_result_request(
    tool_call_id: String,
    function_name: String,
    arguments: String,
) -> async_openai::types::chat::CreateChatCompletionRequest {
    CreateChatCompletionRequestArgs::default()
        .model("fixture-route")
        .messages(vec![
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(ChatCompletionRequestUserMessageContent::Text(
                        "use a tool".to_owned(),
                    ))
                    .build()
                    .expect("user message"),
            ),
            ChatCompletionRequestMessage::Assistant(
                ChatCompletionRequestAssistantMessageArgs::default()
                    .tool_calls(vec![ChatCompletionMessageToolCalls::Function(
                        ChatCompletionMessageToolCall {
                            id: tool_call_id.clone(),
                            function: FunctionCall {
                                name: function_name,
                                arguments,
                            },
                        },
                    )])
                    .build()
                    .expect("assistant tool-call message"),
            ),
            ChatCompletionRequestMessage::Tool(
                ChatCompletionRequestToolMessageArgs::default()
                    .tool_call_id(tool_call_id)
                    .content(ChatCompletionRequestToolMessageContent::Text(
                        "file contents".to_owned(),
                    ))
                    .build()
                    .expect("tool result message"),
            ),
        ])
        .build()
        .expect("tool-result request")
}

#[tokio::test]
async fn manager_backend_server_exposes_site_inventory_and_current_snapshot_without_a_node_local_route()
 {
    let root = support::temporary_real_hardware_fixture();
    configure_remote_node(&root);
    let configuration = support::configuration(&root);
    let snapshot = local_snapshot();
    let (collector, probe) = CountingCollector::new(snapshot.clone());
    let (shutdown, shutdown_signal) = oneshot::channel();
    // A frozen clock never reaches the next wall-aligned hardware refresh, so the collection
    // count only moves when a request collects.
    let clock = dotcore::testing::VirtualClock::starting_at(SystemTime::now());
    let backend = support::prepare_backend_with_collector_and_support(
        configuration,
        async move {
            let _ = shutdown_signal.await;
        },
        dotcore::composition::BackendTestSupport {
            clock: Some(Arc::new(clock)),
            ..dotcore::composition::BackendTestSupport::test_development()
        },
        collector,
    )
    .await
    .expect("prepared backend");
    // The manager node's first collection is registered during preparation, before serving.
    assert_eq!(probe.calls(), 1, "preparation collects hardware once");
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    let response = grpc_web_response(address, list_machines_request(), &mut server).await;
    let mut response: ListMachinesResponse = grpc_web_message(&response);
    // The manager's node is classified from its in-process reports like any node.
    let manager_freshness = response.machines[0]
        .observation_freshness
        .take()
        .expect("manager node freshness");
    assert_eq!(
        manager_freshness.state,
        NodeObservationState::Connected as i32
    );
    let received_at = manager_freshness
        .received_at
        .expect("manager node observation receipt");
    assert!(received_at.seconds >= 1_725_000_000);
    assert_eq!(
        response.machines,
        vec![
            machine("fixture-node", "Fixture node", available()),
            machine("remote-node", "Remote node", unavailable()),
        ]
    );
    assert_eq!(probe.calls(), 1, "listing must not collect hardware");

    let hardware_response =
        grpc_web_response(address, list_hardware_snapshots_request(), &mut server).await;
    let hardware_response: ListHardwareSnapshotsResponse = grpc_web_message(&hardware_response);
    let [snapshot_response, unavailable_response] =
        <[MachineHardwareSnapshot; 2]>::try_from(hardware_response.machines)
            .expect("one hardware entry per configured machine");
    assert_eq!(snapshot_response.machine_id, "fixture-node");
    assert_eq!(snapshot_response.snapshot, Some(snapshot));
    assert!(
        snapshot_response
            .refresh
            .as_ref()
            .and_then(|refresh| refresh.attempted_at.as_ref())
            .is_some_and(|attempted_at| attempted_at.seconds >= 1_725_000_000)
    );
    assert_eq!(
        snapshot_response
            .refresh
            .and_then(|refresh| refresh.outcome),
        Some(hardware_refresh_state::Outcome::Succeeded(Empty {}))
    );
    assert_eq!(
        unavailable_response,
        MachineHardwareSnapshot {
            machine_id: "remote-node".to_owned(),
            snapshot: None,
            refresh: Some(HardwareRefreshState {
                attempted_at: None,
                outcome: Some(hardware_refresh_state::Outcome::Pending(Empty {})),
            }),
            observation_freshness: Some(NodeObservationFreshness {
                state: NodeObservationState::Missing as i32,
                received_at: None,
            }),
        }
    );
    assert_eq!(probe.calls(), 1, "hardware list must not collect hardware");
    assert_unimplemented(grpc_web_response(address, node_local_request(), &mut server).await);
    let diagnostics = grpc_web_response(address, diagnostic_history_request(), &mut server).await;
    let diagnostics: ReadBackendDiagnosticsResponse = grpc_web_message(&diagnostics);
    assert!(diagnostics.events.is_empty());

    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn manager_backend_server_serves_memory_history_from_node_samples() {
    let root = support::temporary_real_hardware_fixture();
    let (collector, _probe) = CountingCollector::new(local_snapshot());
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend_with_snapshot_collector(
        support::configuration(&root),
        async move {
            let _ = shutdown_signal.await;
        },
        collector,
    )
    .await
    .expect("prepared backend");
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    let descriptors: ListMetricDescriptorsResponse = grpc_web_message(
        &grpc_web_response(
            address,
            &grpc_web_request(
                "/dotcore.metrics.v1.MetricsService/ListMetricDescriptors",
                &ListMetricDescriptorsRequest {},
            ),
            &mut server,
        )
        .await,
    );
    let names = descriptors
        .descriptors
        .iter()
        .map(|descriptor| descriptor.name.as_str())
        .collect::<Vec<_>>();
    assert!(
        names.is_sorted(),
        "descriptors are not sorted by name: {names:?}"
    );
    // Names do not depend on the refresh interval the per-collection metrics follow.
    let writers = writer_metric_descriptors(std::time::Duration::from_secs(5));
    validate_catalog(&writers).expect("writer catalog is valid");
    for writer in &writers {
        assert!(
            names.contains(&writer.name.as_str()),
            "composed catalog lacks writer metric {}: {names:?}",
            writer.name
        );
    }
    let dimension = |name: &str, required| MetricDimension {
        name: name.to_owned(),
        required,
    };
    assert_eq!(
        descriptors
            .descriptors
            .iter()
            .find(|descriptor| descriptor.name == "deployment.accelerator.memory.used"),
        Some(&MetricDescriptor {
            name: "deployment.accelerator.memory.used".to_owned(),
            kind: MetricKind::Gauge as i32,
            unit: MetricUnit::Bytes as i32,
            dimensions: vec![
                dimension("deployment", true),
                dimension("node", true),
                dimension("device", false),
            ],
            provenance: MetricProvenance::Node as i32,
            // Recorded once per hardware collection, at the site's default refresh interval.
            sample_interval: Some(dotcore_proto::google::protobuf::Duration {
                seconds: 5,
                nanos: 0,
            }),
            histogram_scale: None,
        })
    );
    let find = |name: &str| {
        descriptors
            .descriptors
            .iter()
            .find(|descriptor| descriptor.name == name)
    };
    assert_eq!(
        find("router.request.ttft"),
        Some(&MetricDescriptor {
            name: "router.request.ttft".to_owned(),
            kind: MetricKind::Distribution as i32,
            unit: MetricUnit::Seconds as i32,
            dimensions: vec![dimension("route", true)],
            provenance: MetricProvenance::Manager as i32,
            sample_interval: None,
            histogram_scale: Some(3),
        })
    );
    assert_eq!(
        find("observability.store.up"),
        Some(&MetricDescriptor {
            name: "observability.store.up".to_owned(),
            kind: MetricKind::Gauge as i32,
            unit: MetricUnit::Ratio as i32,
            dimensions: vec![],
            provenance: MetricProvenance::Manager as i32,
            sample_interval: Some(dotcore_proto::google::protobuf::Duration {
                seconds: 1,
                nanos: 0,
            }),
            histogram_scale: None,
        })
    );
    let signal = |name: &str| {
        descriptors
            .descriptors
            .iter()
            .find(|descriptor| descriptor.name == name)
            .map(|descriptor| {
                (
                    MetricKind::try_from(descriptor.kind).expect("metric kind"),
                    MetricUnit::try_from(descriptor.unit).expect("metric unit"),
                    descriptor
                        .dimensions
                        .iter()
                        .map(|dimension| (dimension.name.as_str(), dimension.required))
                        .collect::<Vec<_>>(),
                    descriptor.sample_interval.map(|interval| interval.seconds),
                )
            })
    };
    let device = vec![("node", true), ("device", true)];
    let filesystem = vec![("node", true), ("mountpoint", true), ("fstype", true)];
    assert_eq!(
        [
            "node.accelerator.utilization",
            "node.accelerator.temperature",
            "node.accelerator.power",
            "node.host.cpu.utilization",
            "node.filesystem.used",
            "node.filesystem.available",
            "node.filesystem.size",
            "router.requests",
            "router.attempts",
        ]
        .map(signal),
        [
            Some((
                MetricKind::Gauge,
                MetricUnit::Ratio,
                device.clone(),
                Some(1)
            )),
            Some((
                MetricKind::Gauge,
                MetricUnit::DegreesCelsius,
                device.clone(),
                Some(1)
            )),
            Some((MetricKind::Gauge, MetricUnit::Watts, device, Some(1))),
            Some((
                MetricKind::Gauge,
                MetricUnit::Ratio,
                vec![("node", true)],
                Some(1)
            )),
            Some((
                MetricKind::Gauge,
                MetricUnit::Bytes,
                filesystem.clone(),
                Some(1)
            )),
            Some((
                MetricKind::Gauge,
                MetricUnit::Bytes,
                filesystem.clone(),
                Some(1)
            )),
            Some((MetricKind::Gauge, MetricUnit::Bytes, filesystem, Some(1))),
            Some((
                MetricKind::Counter,
                MetricUnit::Requests,
                vec![("route", false), ("request_kind", true), ("outcome", true)],
                None
            )),
            Some((
                MetricKind::Counter,
                MetricUnit::Requests,
                vec![
                    ("route", true),
                    ("request_kind", true),
                    ("deployment", true),
                    ("model", false),
                    ("node", false),
                    ("target_kind", true),
                    ("outcome", true),
                    ("stage", false),
                ],
                None
            )),
        ]
    );
    assert_eq!(
        descriptors
            .resolutions
            .iter()
            .map(|resolution| resolution.step.expect("resolution step").seconds)
            .collect::<Vec<_>>(),
        vec![1, 10, 60]
    );

    let used_memory = QueryMetricsRequest {
        query: Some(MetricQuery {
            metric: "node.accelerator.memory.used".to_owned(),
            group_by: vec!["node".to_owned(), "device".to_owned()],
            reduce: MetricReduce::Last as i32,
            aggregate: MetricAggregate::Sum as i32,
            range: Some(metric_query::Range::Last(
                dotcore_proto::google::protobuf::Duration {
                    seconds: 60,
                    nanos: 0,
                },
            )),
            step_hint: Some(dotcore_proto::google::protobuf::Duration {
                seconds: 1,
                nanos: 0,
            }),
            ..MetricQuery::default()
        }),
    };
    // The node samples at each wall second; its forwarder flushes a bucket once the next one
    // starts, so history appears within about two seconds of serving.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let result = loop {
        let result = grpc_web_message::<QueryMetricsResponse>(
            &grpc_web_response(
                address,
                &grpc_web_request(
                    "/dotcore.metrics.v1.MetricsService/QueryMetrics",
                    &used_memory,
                ),
                &mut server,
            )
            .await,
        )
        .result
        .expect("query result");
        if result
            .series
            .first()
            .is_some_and(|series| series.points.len() >= 2)
        {
            break result;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no sampled memory history before the deadline: {result:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(result.effective_step.expect("effective step").seconds, 1);
    assert_eq!(result.series.len(), 1);
    let series = &result.series[0];
    assert_eq!(
        series.labels,
        [
            ("device".to_owned(), "GPU-fixture".to_owned()),
            ("node".to_owned(), "fixture-node".to_owned()),
        ]
        .into()
    );
    // One point per second, each the sampled device reading.
    let starts: Vec<i64> = series
        .points
        .iter()
        .map(|point| point.bucket_start.expect("bucket start").seconds)
        .collect();
    assert!(
        starts.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "{starts:?}"
    );
    assert!(
        series
            .points
            .iter()
            .all(|point| point.value == Some(metric_point::Value::Gauge(6_000.0)))
    );

    let mut unknown = used_memory;
    unknown.query.as_mut().expect("query").metric = "node.unknown".to_owned();
    assert_grpc_status(
        &grpc_web_response(
            address,
            &grpc_web_request("/dotcore.metrics.v1.MetricsService/QueryMetrics", &unknown),
            &mut server,
        )
        .await,
        5,
        "metric node.unknown is not declared",
    );

    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn manager_backend_server_serves_distribution_percentiles() {
    let root = support::temporary_fast_fixture();
    let clock = dotcore::testing::ManualClock::new();
    let recorder = dotcore::composition::MetricRecorderSlot::default();
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend_with_test_support(
        support::configuration(&root),
        async move {
            let _ = shutdown_signal.await;
        },
        dotcore::composition::BackendTestSupport {
            clock: Some(Arc::new(clock.clone())),
            metric_recorder: Some(recorder.clone()),
            ..dotcore::composition::BackendTestSupport::test()
        },
    )
    .await
    .expect("prepared backend");
    // Samples are observed on the backend's clock, so they never lie past the end of the window
    // the query resolves from that clock.
    let observed_at = clock.wall_now();
    recorder
        .recorder()
        .expect("composed metric recorder")
        .record(
            [0.125, 0.25, 0.375, 0.75, 1.5]
                .into_iter()
                .map(|seconds| {
                    dotcore_observability::MetricRecord::distribution(
                        "router.request.ttft",
                        dotcore_observability::labels::LabelSet::new().with("route", "route-a"),
                        observed_at,
                        seconds,
                    )
                })
                .collect(),
        );
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    let descriptors: ListMetricDescriptorsResponse = grpc_web_message(
        &grpc_web_response(
            address,
            &grpc_web_request(
                "/dotcore.metrics.v1.MetricsService/ListMetricDescriptors",
                &ListMetricDescriptorsRequest {},
            ),
            &mut server,
        )
        .await,
    );
    assert_eq!(
        descriptors
            .descriptors
            .iter()
            .find(|descriptor| descriptor.name == "router.request.ttft"),
        Some(&MetricDescriptor {
            name: "router.request.ttft".to_owned(),
            kind: MetricKind::Distribution as i32,
            unit: MetricUnit::Seconds as i32,
            dimensions: vec![MetricDimension {
                name: "route".to_owned(),
                required: true,
            }],
            provenance: MetricProvenance::Manager as i32,
            sample_interval: None,
            histogram_scale: Some(3),
        })
    );

    let query = QueryMetricsRequest {
        query: Some(MetricQuery {
            metric: "router.request.ttft".to_owned(),
            group_by: vec!["route".to_owned()],
            reduce: MetricReduce::Last as i32,
            aggregate: MetricAggregate::Sum as i32,
            percentiles: vec![0.5, 0.95],
            range: Some(metric_query::Range::Last(
                dotcore_proto::google::protobuf::Duration {
                    seconds: 60,
                    nanos: 0,
                },
            )),
            step_hint: Some(dotcore_proto::google::protobuf::Duration {
                seconds: 60,
                nanos: 0,
            }),
            ..MetricQuery::default()
        }),
    };
    let result = grpc_web_message::<QueryMetricsResponse>(
        &grpc_web_response(
            address,
            &grpc_web_request("/dotcore.metrics.v1.MetricsService/QueryMetrics", &query),
            &mut server,
        )
        .await,
    )
    .result
    .expect("query result");

    // The nearest-rank samples 0.375 and 1.5 fall in scale-3 buckets -11 and 5; each estimate is
    // its bucket's geometric midpoint.
    let midpoint = |index: f64| (((index - 1.0) / 8.0).exp2() * (index / 8.0).exp2()).sqrt();
    let minute = observed_at
        .duration_since(UNIX_EPOCH)
        .expect("after epoch")
        .as_secs()
        / 60
        * 60;
    assert_eq!(result.effective_step.expect("effective step").seconds, 60);
    assert_eq!(result.series.len(), 1);
    assert_eq!(
        result.series[0].labels,
        [("route".to_owned(), "route-a".to_owned())].into()
    );
    let points = &result.series[0].points;
    assert_eq!(points.len(), 1);
    assert_eq!(
        points[0].bucket_start.expect("bucket start").seconds,
        minute as i64
    );
    assert_eq!(
        points[0].value,
        Some(metric_point::Value::Distribution(MetricDistributionValue {
            count: 5,
            sum: 3.0,
            min: 0.125,
            max: 1.5,
            mean: 0.6,
            percentiles: vec![
                MetricPercentile {
                    quantile: 0.5,
                    value: midpoint(-11.0),
                },
                MetricPercentile {
                    quantile: 0.95,
                    value: midpoint(5.0),
                },
            ],
        }))
    );

    let mut invalid = query;
    invalid.query.as_mut().expect("query").percentiles = vec![1.5];
    assert_grpc_status(
        &grpc_web_response(
            address,
            &grpc_web_request("/dotcore.metrics.v1.MetricsService/QueryMetrics", &invalid),
            &mut server,
        )
        .await,
        3,
        "metric query percentile 1.5 is not between 0 and 1",
    );

    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn manager_backend_server_streams_metric_watches() {
    let root = support::temporary_fast_fixture();
    let clock = dotcore::testing::ManualClock::new();
    let recorder = dotcore::composition::MetricRecorderSlot::default();
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend_with_test_support(
        support::configuration(&root),
        async move {
            let _ = shutdown_signal.await;
        },
        dotcore::composition::BackendTestSupport {
            clock: Some(Arc::new(clock.clone())),
            metric_recorder: Some(recorder.clone()),
            ..dotcore::composition::BackendTestSupport::test()
        },
    )
    .await
    .expect("prepared backend");
    let recorder = recorder.recorder().expect("composed metric recorder");
    let ttft = |observed_at, seconds| {
        dotcore_observability::MetricRecord::distribution(
            "router.request.ttft",
            dotcore_observability::labels::LabelSet::new().with("route", "route-a"),
            observed_at,
            seconds,
        )
    };
    let backfilled_at = clock.wall_now();
    recorder.record(vec![ttft(backfilled_at, 0.25), ttft(backfilled_at, 0.75)]);
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    let ttft_query = |key: &str, step_seconds| KeyedMetricQuery {
        key: key.to_owned(),
        query: Some(MetricQuery {
            metric: "router.request.ttft".to_owned(),
            group_by: vec!["route".to_owned()],
            reduce: MetricReduce::Last as i32,
            aggregate: MetricAggregate::Sum as i32,
            percentiles: vec![0.5],
            range: Some(metric_query::Range::Last(
                dotcore_proto::google::protobuf::Duration {
                    seconds: 60,
                    nanos: 0,
                },
            )),
            step_hint: Some(dotcore_proto::google::protobuf::Duration {
                seconds: step_seconds,
                nanos: 0,
            }),
            ..MetricQuery::default()
        }),
    };
    let request = WatchMetricsRequest {
        queries: vec![ttft_query("fine", 1), ttft_query("coarse", 10)],
    };
    let mut watch = GrpcWebStream::open(
        address,
        &grpc_web_request("/dotcore.metrics.v1.MetricsService/WatchMetrics", &request),
        &mut server,
    )
    .await;

    let backfill = |response: WatchMetricsResponse| {
        let Some(watch_metrics_response::Event::Upsert(upsert)) = response.event else {
            panic!("each query opens with a backfill upsert");
        };
        (upsert.key, upsert.result.expect("backfill result"))
    };
    let (fine_key, fine) = backfill(watch.message::<WatchMetricsResponse>().await);
    assert_eq!(fine_key, "fine");
    assert_eq!(fine.effective_step.expect("effective step").seconds, 1);
    assert_eq!(fine.series.len(), 1);
    assert_eq!(
        fine.series[0].labels,
        [("route".to_owned(), "route-a".to_owned())].into()
    );
    let points = &fine.series[0].points;
    assert_eq!(points.len(), 1);
    assert_eq!(points[0].bucket_start, Some(whole_second(backfilled_at)));
    let backfilled_distribution =
        Some(metric_point::Value::Distribution(MetricDistributionValue {
            count: 2,
            sum: 1.0,
            min: 0.25,
            max: 0.75,
            mean: 0.5,
            percentiles: vec![MetricPercentile {
                quantile: 0.5,
                value: 0.25,
            }],
        }));
    // The nearest-rank median is the smaller sample, kept within the bucket's minimum.
    assert_eq!(points[0].value, backfilled_distribution);

    let (coarse_key, coarse) = backfill(watch.message::<WatchMetricsResponse>().await);
    assert_eq!(coarse_key, "coarse");
    assert_eq!(coarse.effective_step.expect("effective step").seconds, 10);
    assert_eq!(coarse.incarnation, fine.incarnation);
    let coarse_points = &coarse.series[0].points;
    assert_eq!(coarse_points.len(), 1);
    let ten_seconds = whole_second(backfilled_at).seconds / 10 * 10;
    assert_eq!(
        coarse_points[0].bucket_start,
        Some(Timestamp {
            seconds: ten_seconds,
            nanos: 0
        })
    );
    assert_eq!(coarse_points[0].value, backfilled_distribution);

    assert_eq!(
        watch.message::<WatchMetricsResponse>().await.event,
        Some(watch_metrics_response::Event::BackfillComplete(Empty {}))
    );

    clock.advance(Duration::from_secs(2));
    let live_at = clock.wall_now();
    recorder.record(vec![ttft(live_at, 0.5)]);
    // Past the bucket's end and its closing grace, so the live point arrives closed.
    clock.advance(Duration::from_secs(2));
    let live_bucket = Some(whole_second(live_at));
    let live = loop {
        let Some(watch_metrics_response::Event::Upsert(upsert)) =
            watch.message::<WatchMetricsResponse>().await.event
        else {
            panic!("live watch messages are upserts");
        };
        assert!(
            ["fine", "coarse"].contains(&upsert.key.as_str()),
            "live upserts carry a requested key, got {:?}",
            upsert.key
        );
        let upsert_result = upsert.result.expect("live result");
        assert_eq!(upsert_result.incarnation, fine.incarnation);
        if upsert.key != "fine" {
            continue;
        }
        let closed = upsert_result
            .series
            .iter()
            .flat_map(|series| &series.points)
            .find(|point| point.bucket_start == live_bucket && !point.provisional)
            .cloned();
        if let Some(point) = closed {
            break point;
        }
    };
    assert_eq!(
        live,
        MetricPoint {
            bucket_start: live_bucket,
            provisional: false,
            value: Some(metric_point::Value::Distribution(MetricDistributionValue {
                count: 1,
                sum: 0.5,
                min: 0.5,
                max: 0.5,
                mean: 0.5,
                percentiles: vec![MetricPercentile {
                    quantile: 0.5,
                    value: 0.5,
                }],
            })),
        }
    );

    drop(watch);
    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn manager_backend_server_streams_resource_watches() {
    let root = support::temporary_fast_fixture();
    let clock = dotcore::testing::ManualClock::new();
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend_with_test_support(
        support::configuration(&root),
        async move {
            let _ = shutdown_signal.await;
        },
        dotcore::composition::BackendTestSupport {
            clock: Some(Arc::new(clock.clone())),
            simulated_runtime: Some(SimulatedRuntimeTestSupport::with_stop_failures(0)),
            ..dotcore::composition::BackendTestSupport::test()
        },
    )
    .await
    .expect("prepared backend");
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    // Created before the clock moves: a collection stamped ahead of real time reads as stale.
    let mut models = ModelDeploymentServiceClient::connect(format!("http://{address}"))
        .await
        .expect("model API connection");
    let deployment_id = models
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
        .expect("created deployment")
        .model_deployment_id;
    let subscription = |key: &str, kind| ResourceSubscription {
        key: key.to_owned(),
        kind: Some(kind),
    };
    let request = WatchResourcesRequest {
        subscriptions: vec![
            subscription(
                "models",
                resource_subscription::Kind::ModelDeployments(Default::default()),
            ),
            subscription(
                "routes",
                resource_subscription::Kind::Routes(Default::default()),
            ),
            subscription(
                "console",
                resource_subscription::Kind::ConsoleSettings(Default::default()),
            ),
        ],
    };
    let mut watch = GrpcWebStream::open(
        address,
        &grpc_web_request(
            "/dotcore.watch.v1.ResourceWatchService/WatchResources",
            &request,
        ),
        &mut server,
    )
    .await;

    // First round: one full payload per key, then the completion marker.
    let mut first_round = Vec::new();
    let (session_id, first_tick) = loop {
        let response = next_resource_event(&mut watch, &clock).await;
        match response.event.expect("watch event") {
            watch_resources_response::Event::Resource(resource) => first_round.push(resource),
            watch_resources_response::Event::SnapshotComplete(_) => {
                break (response.manager_session_id, response.tick_sequence);
            }
            watch_resources_response::Event::Heartbeat(_) => panic!("heartbeat in first round"),
        }
    };
    assert_eq!(session_id.len(), 32);
    // The first round is read on open, before any tick.
    assert_eq!(first_tick, 0);
    first_round.sort_by(|left, right| left.key.cmp(&right.key));
    let [console_round, models_round, routes_round] = first_round.as_slice() else {
        panic!("one payload per key in the first round: {first_round:?}");
    };
    assert_eq!(models_round.key, "models");
    let Some(Payload::ModelDeployments(listed)) = &models_round.payload else {
        panic!("model deployments payload: {models_round:?}");
    };
    let listed_ids: Vec<_> = listed
        .deployments
        .iter()
        .map(|deployment| deployment.model_deployment_id.as_str())
        .collect();
    assert_eq!(listed_ids, [deployment_id.as_str()]);
    assert_eq!(
        routes_round,
        &KeyedResource {
            key: "routes".to_owned(),
            payload: Some(Payload::Routes(Default::default())),
        }
    );
    assert_eq!(
        console_round,
        &KeyedResource {
            key: "console".to_owned(),
            payload: Some(Payload::ConsoleSettings(GetConsoleSettingsResponse {
                settings: Some(ConsoleSettings::default()),
            })),
        }
    );

    // A pause reaches the watch as an update of the changed kind alone.
    models
        .pause_model(PauseModelRequest {
            model_deployment_id: deployment_id.clone(),
        })
        .await
        .expect("pause model deployment");
    let update = loop {
        let response = next_resource_event(&mut watch, &clock).await;
        let Some(watch_resources_response::Event::Resource(resource)) = response.event else {
            panic!("an update follows the first round: {response:?}");
        };
        assert_eq!(resource.key, "models", "only the changed kind is published");
        let Some(Payload::ModelDeployments(list)) = resource.payload else {
            panic!("model deployments payload");
        };
        if list.deployments.iter().any(|deployment| {
            deployment.model_deployment_id == deployment_id
                && deployment.desired_state == ModelDeploymentDesiredState::Paused as i32
        }) {
            break response.tick_sequence;
        }
    };
    assert!(
        update > first_tick,
        "update tick {update} follows {first_tick}"
    );

    // A console settings update reaches the watch as the accepted settings.
    let hidden = ConsoleSettings {
        get_started_hidden: true,
    };
    let accepted = SiteServiceClient::connect(format!("http://{address}"))
        .await
        .expect("site API connection")
        .update_console_settings(UpdateConsoleSettingsRequest {
            settings: Some(hidden),
            update_mask: Some(FieldMask {
                paths: vec!["get_started_hidden".to_owned()],
            }),
        })
        .await
        .expect("update console settings")
        .into_inner();
    assert_eq!(accepted.settings, Some(hidden));
    let console_update = loop {
        let response = next_resource_event(&mut watch, &clock).await;
        let Some(watch_resources_response::Event::Resource(resource)) = response.event else {
            panic!("a console settings update follows: {response:?}");
        };
        assert_ne!(resource.key, "routes", "routes are unchanged");
        if resource.key == "console" {
            assert_eq!(
                resource.payload,
                Some(Payload::ConsoleSettings(GetConsoleSettingsResponse {
                    settings: Some(hidden),
                }))
            );
            break response.tick_sequence;
        }
    };
    // A tick reads every watched kind at once, so the console read of the pause's tick may
    // already see the update and publish it under that same tick.
    assert!(
        console_update >= update,
        "console update tick {console_update} is not before {update}"
    );

    // An idle stream heartbeats once fifteen seconds pass without an event.
    clock.advance(Duration::from_secs(15));
    let heartbeat = loop {
        let response = watch.message::<WatchResourcesResponse>().await;
        if let Some(watch_resources_response::Event::Heartbeat(_)) = response.event {
            break response;
        }
    };
    assert_eq!(heartbeat.manager_session_id, session_id);
    assert!(heartbeat.tick_sequence >= console_update);

    drop(watch);
    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

/// Next watch response, advancing the manual clock one second at a time until a tick publishes.
async fn next_resource_event(
    watch: &mut GrpcWebStream,
    clock: &dotcore::testing::ManualClock,
) -> WatchResourcesResponse {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        clock.advance(Duration::from_secs(1));
        let message = tokio::time::timeout(
            Duration::from_millis(100),
            watch.message::<WatchResourcesResponse>(),
        )
        .await;
        if let Ok(response) = message {
            return response;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "resource watch event deadline"
        );
    }
}

#[tokio::test]
async fn a_manager_node_target_admits_a_create_right_after_start_without_a_private_listener() {
    let root = support::temporary_fast_fixture();
    let backend = support::RunningBackend::start(support::configuration(&root)).await;

    let deployment = backend
        .model_client()
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
        .expect("create on the manager node's target right after start")
        .into_inner()
        .deployment
        .expect("created deployment");

    assert_eq!(
        deployment
            .intent
            .and_then(|intent| intent.placement)
            .map(|placement| (placement.site_node_id, placement.execution_target_id)),
        Some(("fixture-node".to_owned(), "fixture-target".to_owned()))
    );
    backend.stop().await;
}

#[tokio::test]
async fn backend_preparation_waits_for_the_manager_node_first_collection() {
    let root = support::temporary_real_hardware_fixture();
    let configuration = support::configuration(&root);
    let (collector, probe, release) = BlockingCollector::new(local_snapshot());
    let (shutdown, shutdown_signal) = oneshot::channel();
    let mut preparation = tokio::spawn(support::prepare_backend_with_snapshot_collector(
        configuration,
        async move {
            let _ = shutdown_signal.await;
        },
        collector,
    ));
    probe.wait_for_calls(1).await;
    tokio::task::yield_now().await;
    assert!(
        !preparation.is_finished(),
        "preparation must wait for the first collection"
    );

    release.send(()).expect("release first collection");
    let backend = (&mut preparation)
        .await
        .expect("preparation task")
        .expect("prepared backend");
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());
    let response = fixture_node_hardware(address, &mut server).await;

    assert_eq!(response.snapshot, Some(local_snapshot()));
    assert_eq!(
        response.refresh.and_then(|refresh| refresh.outcome),
        Some(hardware_refresh_state::Outcome::Succeeded(Empty {}))
    );
    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

#[tokio::test]
async fn backend_reports_exact_unconfigured_accelerator_discovery_outcomes() {
    let cases = vec![
        (
            "detected",
            FixedDiscovery::Detected(local_accelerator_facts()),
            accelerator_observation::Result::Facts(AcceleratorFactsSet {
                facts: local_accelerator_facts(),
            }),
        ),
        (
            "absent",
            FixedDiscovery::Absent,
            accelerator_observation::Result::Facts(AcceleratorFactsSet { facts: Vec::new() }),
        ),
        (
            "unavailable",
            FixedDiscovery::Unavailable(UnavailableReason::MissingLibrary),
            accelerator_observation::Result::Unavailable(ObservationUnavailable {
                code: "MISSING_LIBRARY".to_owned(),
                message: "NVIDIA observation is unavailable because its library is not available."
                    .to_owned(),
            }),
        ),
    ];

    for (label, discovery, expected_result) in cases {
        let root = support::temporary_real_hardware_fixture();
        let site = std::fs::read_to_string(root.join("site.yaml")).expect("Site fixture");
        assert!(
            !site.to_ascii_lowercase().contains("nvidia"),
            "{label} case must use a site without NVIDIA configuration"
        );
        let discovery_calls = Arc::new(AtomicUsize::new(0));
        let collector = HardwareSnapshotCollector::new(
            FixedHostFacts,
            vec![Box::new(FixedDiscoverer {
                calls: Arc::clone(&discovery_calls),
                outcome: Mutex::new(Some(discovery)),
            })],
            Arc::new(FixedObservationClock),
        );
        let (shutdown, shutdown_signal) = oneshot::channel();
        let backend = support::prepare_backend_with_snapshot_collector(
            support::configuration(&root),
            async move {
                let _ = shutdown_signal.await;
            },
            collector,
        )
        .await
        .expect("prepared backend");
        assert_eq!(
            discovery_calls.load(Ordering::SeqCst),
            1,
            "{label} discovery runs once during preparation"
        );
        let address = backend.addresses().management.expect("management listener");
        let mut server = tokio::spawn(backend.serve());

        let response = fixture_node_hardware(address, &mut server).await;

        assert_eq!(
            response.snapshot,
            Some(HardwareSnapshot {
                observed_at: Some(fixed_observed_at()),
                host: Some(HostObservation {
                    result: Some(host_observation::Result::Facts(local_host_facts())),
                }),
                accelerators: vec![AcceleratorObservation {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    result: Some(expected_result),
                }],
            }),
            "{label} discovery outcome"
        );
        assert_eq!(
            response.refresh.and_then(|refresh| refresh.outcome),
            Some(hardware_refresh_state::Outcome::Succeeded(Empty {})),
            "{label} refresh outcome"
        );
        assert_eq!(discovery_calls.load(Ordering::SeqCst), 1);

        shutdown.send(()).expect("server shutdown");
        join_server(server).await;
    }
}

async fn grpc_web_response(
    address: std::net::SocketAddr,
    request: &[u8],
    server: &mut tokio::task::JoinHandle<Result<(), CompositionError>>,
) -> Vec<u8> {
    let mut stream = management_connection(address, server).await;
    stream.write_all(request).await.expect("gRPC-Web request");
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("gRPC-Web response timeout")
        .expect("gRPC-Web response");
    response
}

async fn management_connection(
    address: std::net::SocketAddr,
    server: &mut tokio::task::JoinHandle<Result<(), CompositionError>>,
) -> tokio::net::TcpStream {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            tokio::select! {
                result = &mut *server => {
                    let error = result.expect("server task").expect_err("server must remain active");
                    panic!("server failed before management connection: {error}");
                }
                result = tokio::net::TcpStream::connect(address) => match result {
                    Ok(stream) => break stream,
                    Err(_) => tokio::task::yield_now().await,
                },
            }
        }
    })
    .await
    .expect("management connection timeout")
}

/// Reads the gRPC-Web frames of one streamed management response as they arrive.
struct GrpcWebStream {
    connection: tokio::net::TcpStream,

    /// Received chunked body bytes not yet split into chunks.
    chunked: Vec<u8>,

    /// Chunk payload bytes not yet split into gRPC-Web frames.
    frames: Vec<u8>,
}

impl GrpcWebStream {
    async fn open(
        address: std::net::SocketAddr,
        request: &[u8],
        server: &mut tokio::task::JoinHandle<Result<(), CompositionError>>,
    ) -> Self {
        let mut stream = Self {
            connection: management_connection(address, server).await,
            chunked: Vec::new(),
            frames: Vec::new(),
        };
        stream
            .connection
            .write_all(request)
            .await
            .expect("gRPC-Web request");
        let headers_end = loop {
            if let Some(end) = stream
                .chunked
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                break end + 4;
            }
            stream.read().await;
        };
        let headers: Vec<u8> = stream.chunked.drain(..headers_end).collect();
        assert!(
            headers.starts_with(b"HTTP/1.1 200"),
            "streamed response headers: {}",
            String::from_utf8_lossy(&headers)
        );
        stream
    }

    /// Waits for the next data frame and decodes it; a trailer frame fails with its status.
    async fn message<M: Message + Default>(&mut self) -> M {
        loop {
            if self.frames.len() >= 5 {
                let length =
                    u32::from_be_bytes(self.frames[1..5].try_into().expect("frame length"));
                let frame_end = 5 + length as usize;
                if self.frames.len() >= frame_end {
                    let frame: Vec<u8> = self.frames.drain(..frame_end).collect();
                    assert_eq!(
                        frame[0],
                        0,
                        "gRPC-Web data frame, not trailers: {}",
                        String::from_utf8_lossy(&frame[5..])
                    );
                    return M::decode(&frame[5..]).expect("gRPC-Web protobuf message");
                }
            }
            if !self.take_chunk() {
                self.read().await;
            }
        }
    }

    /// Moves one complete chunk's payload into the frame buffer; false when none is complete.
    fn take_chunk(&mut self) -> bool {
        let Some(line_end) = self.chunked.windows(2).position(|window| window == b"\r\n") else {
            return false;
        };
        let length = usize::from_str_radix(
            std::str::from_utf8(&self.chunked[..line_end]).expect("chunk length"),
            16,
        )
        .expect("hex chunk length");
        assert_ne!(length, 0, "the streamed response ended");
        let chunk_end = line_end + 2 + length + 2;
        if self.chunked.len() < chunk_end {
            return false;
        }
        self.frames
            .extend_from_slice(&self.chunked[line_end + 2..chunk_end - 2]);
        self.chunked.drain(..chunk_end);
        true
    }

    async fn read(&mut self) {
        let mut buffer = [0_u8; 4096];
        let read = tokio::time::timeout(Duration::from_secs(10), self.connection.read(&mut buffer))
            .await
            .expect("streamed response deadline")
            .expect("streamed response read");
        assert_ne!(read, 0, "the management connection closed mid-stream");
        self.chunked.extend_from_slice(&buffer[..read]);
    }
}

fn whole_second(time: SystemTime) -> Timestamp {
    Timestamp {
        seconds: time
            .duration_since(UNIX_EPOCH)
            .expect("after epoch")
            .as_secs() as i64,
        nanos: 0,
    }
}

#[tokio::test]
async fn prepared_backend_retains_exact_port_zero_listeners() {
    let root = support::temporary_fast_fixture();
    let backend = support::prepare_backend(support::configuration(&root), std::future::pending())
        .await
        .expect("prepared backend");
    let addresses = backend.addresses();

    assert_ne!(addresses.management.expect("management listener").port(), 0);
    assert_ne!(addresses.inference.expect("inference listener").port(), 0);
    tokio::net::TcpListener::bind(addresses.management.expect("management listener"))
        .await
        .expect_err("prepared backend must retain management listener");
    tokio::net::TcpListener::bind(addresses.inference.expect("inference listener"))
        .await
        .expect_err("prepared backend must retain inference listener");
}

#[tokio::test]
async fn dropping_a_prepared_backend_releases_both_listeners_without_serving() {
    let root = support::temporary_fast_fixture();
    let management_port = dotcore_domain::test_support::ports::reserve_port();
    let inference_port = dotcore_domain::test_support::ports::reserve_port();
    let backend = support::prepare_backend_on_ports(
        support::configuration(&root),
        management_port.port(),
        inference_port.port(),
        std::future::pending(),
    )
    .await
    .expect("prepared backend");
    let addresses = backend.addresses();

    drop(backend);

    tokio::net::TcpListener::bind(addresses.management.expect("management listener"))
        .await
        .expect("released management listener");
    tokio::net::TcpListener::bind(addresses.inference.expect("inference listener"))
        .await
        .expect("released inference listener");
}

#[tokio::test]
async fn site_without_application_target_serves_management_traffic() {
    let root = support::temporary_fast_fixture();
    let path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&path).expect("Site fixture");
    site.execution_targets.retain(|target| {
        !matches!(
            target.runtime_binding,
            Some(dotcore_proto::dotcore::site::v1::execution_target::RuntimeBinding::ApplicationRuntime(_))
        )
    });
    std::fs::write(
        path,
        dotcore_proto::yaml::to_string(&site).expect("Site without application runtime YAML"),
    )
    .expect("Site without application runtime");
    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend(support::configuration(&root), async move {
        let _ = shutdown_signal.await;
    })
    .await
    .expect("Backend without application runtime");
    let address = backend.addresses().management.expect("management listener");
    let mut server = tokio::spawn(backend.serve());

    let response = grpc_web_response(address, list_machines_request(), &mut server).await;
    assert!(response.starts_with(b"HTTP/1.1 200"));

    shutdown.send(()).expect("server shutdown");
    join_server(server).await;
}

async fn join_server(server: tokio::task::JoinHandle<Result<(), CompositionError>>) {
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server join deadline")
        .expect("server task")
        .expect("server result");
}

fn assert_unimplemented(response: Vec<u8>) {
    assert_grpc_status(&response, 12, "");
}

fn assert_grpc_status(response: &[u8], code: u32, message: &str) {
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let response = String::from_utf8_lossy(response);
    assert!(response.contains(&format!("grpc-status: {code}\r\n")));
    if !message.is_empty() {
        let encoded_message = message.replace(' ', "%20");
        assert!(response.contains(&format!("grpc-message: {encoded_message}\r\n")));
    }
}

fn grpc_web_message<M: Message + Default>(response: &[u8]) -> M {
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let headers_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("HTTP response headers")
        + 4;
    let body = &response[headers_end..];
    let chunk_end = body
        .windows(2)
        .position(|window| window == b"\r\n")
        .expect("HTTP response chunk");
    let chunk_length = usize::from_str_radix(
        std::str::from_utf8(&body[..chunk_end]).expect("chunk length"),
        16,
    )
    .expect("hex chunk length");
    let chunk_start = chunk_end + 2;
    let chunk = &body[chunk_start..chunk_start + chunk_length];
    assert_eq!(chunk[0], 0, "first gRPC-Web frame is a data frame");
    let message_length = u32::from_be_bytes(chunk[1..5].try_into().expect("frame length"));
    assert_eq!(message_length as usize, chunk.len() - 5);

    let trailer_chunk = &body[chunk_start + chunk_length + 2..];
    let trailer_chunk_end = trailer_chunk
        .windows(2)
        .position(|window| window == b"\r\n")
        .expect("gRPC-Web trailer chunk");
    let trailer_length = usize::from_str_radix(
        std::str::from_utf8(&trailer_chunk[..trailer_chunk_end]).expect("trailer chunk length"),
        16,
    )
    .expect("hex trailer chunk length");
    let trailer_start = trailer_chunk_end + 2;
    let trailer = &trailer_chunk[trailer_start..trailer_start + trailer_length];
    assert_eq!(trailer[0], 0x80, "second gRPC-Web frame is a trailer frame");
    let trailer_message_length =
        u32::from_be_bytes(trailer[1..5].try_into().expect("trailer frame length"));
    assert_eq!(trailer_message_length as usize, trailer.len() - 5);
    assert_eq!(
        &trailer[5..],
        b"grpc-status:0\r\n",
        "successful gRPC-Web response trailer"
    );
    M::decode(&chunk[5..]).expect("gRPC-Web protobuf response")
}

fn machine(
    machine_id: &str,
    display_name: &str,
    availability: CurrentHardwareSnapshotAvailability,
) -> Machine {
    let (freshness, runtimes) = match machine_id {
        // On its real hardware, the fixture node holds only its application target.
        "fixture-node" => (
            None,
            vec![RuntimeObservation {
                execution_target_id: "fixture-application-target".to_owned(),
                kind: RuntimeKind::Application as i32,
                result: Some(runtime_observation::Result::Available(Empty {})),
            }],
        ),
        "remote-node" => (
            Some(NodeObservationFreshness {
                state: NodeObservationState::Missing as i32,
                received_at: None,
            }),
            Vec::new(),
        ),
        _ => panic!("unexpected machine fixture {machine_id}"),
    };
    Machine {
        machine_id: machine_id.to_owned(),
        display_name: display_name.to_owned(),
        site_manager: machine_id == "fixture-node",
        current_hardware_snapshot_availability: Some(availability),
        observation_freshness: freshness,
        runtimes,
    }
}

fn available() -> CurrentHardwareSnapshotAvailability {
    CurrentHardwareSnapshotAvailability {
        availability: Some(
            current_hardware_snapshot_availability::Availability::Available(Empty {}),
        ),
    }
}

fn unavailable() -> CurrentHardwareSnapshotAvailability {
    CurrentHardwareSnapshotAvailability {
        availability: Some(
            current_hardware_snapshot_availability::Availability::Unavailable(
                ObservationUnavailable {
                    code: "CURRENT_HARDWARE_SNAPSHOT_UNAVAILABLE".to_owned(),
                    message: "A current hardware snapshot is unavailable for this machine."
                        .to_owned(),
                },
            ),
        ),
    }
}

fn diagnostic_history_request() -> &'static [u8] {
    b"POST /dotcore.diagnostic.v1.DiagnosticService/ReadBackendDiagnostics HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: 8\r\n\r\n\0\0\0\0\x03\x08\xc8\x01"
}

fn grpc_web_request(path: &str, message: &impl Message) -> Vec<u8> {
    let body = message.encode_to_vec();
    let frame_length = body.len() + 5;
    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: {frame_length}\r\n\r\n"
    )
    .into_bytes();
    request.push(0);
    request.extend(
        u32::try_from(body.len())
            .expect("message length")
            .to_be_bytes(),
    );
    request.extend(body);
    request
}

fn list_machines_request() -> &'static [u8] {
    b"POST /dotcore.site.v1.SiteService/ListMachines HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0"
}

fn node_local_request() -> &'static [u8] {
    b"POST /dotcore.node.v1.NodeCapability/CollectHardwareSnapshot HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0"
}

/// The fixture node's entry of the hardware snapshot list, read over gRPC-Web.
async fn fixture_node_hardware(
    address: std::net::SocketAddr,
    server: &mut tokio::task::JoinHandle<Result<(), CompositionError>>,
) -> MachineHardwareSnapshot {
    let response = grpc_web_response(address, list_hardware_snapshots_request(), server).await;
    let response: ListHardwareSnapshotsResponse = grpc_web_message(&response);
    response
        .machines
        .into_iter()
        .find(|machine| machine.machine_id == "fixture-node")
        .expect("fixture node hardware")
}

fn list_hardware_snapshots_request() -> &'static [u8] {
    b"POST /dotcore.site.v1.SiteService/ListHardwareSnapshots HTTP/1.1\r\nHost: localhost\r\nOrigin: http://127.0.0.1:5173\r\nContent-Type: application/grpc-web+proto\r\nx-grpc-web: 1\r\nConnection: close\r\nContent-Length: 5\r\n\r\n\0\0\0\0\0"
}

enum FixedDiscovery {
    Detected(Vec<AcceleratorFacts>),
    Absent,
    Unavailable(UnavailableReason),
}

struct FixedDiscoverer {
    calls: Arc<AtomicUsize>,
    outcome: Mutex<Option<FixedDiscovery>>,
}

impl AcceleratorVendorDiscoverer for FixedDiscoverer {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn discover(&self) -> AcceleratorDiscovery {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.outcome.lock().expect("discovery outcome").take() {
            Some(FixedDiscovery::Detected(facts)) => {
                AcceleratorDiscovery::Detected(Box::new(FixedPoller { facts }))
            }
            Some(FixedDiscovery::Absent) => AcceleratorDiscovery::Absent,
            Some(FixedDiscovery::Unavailable(reason)) => AcceleratorDiscovery::Unavailable(reason),
            None => panic!("discoverer must run exactly once"),
        }
    }
}

struct FixedPoller {
    facts: Vec<AcceleratorFacts>,
}

impl AcceleratorVendorPoller for FixedPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        Ok(AcceleratorVendorObservation {
            facts: self.facts.clone(),
            processes: Vec::new(),
        })
    }
}

struct FixedHostFacts;

impl HostFactsAdapter for FixedHostFacts {
    fn collect_host_facts(&self) -> Result<HostFacts, UnavailableReason> {
        Ok(local_host_facts())
    }
}

/// Tokio monotonic time with wall time fixed at [`fixed_observed_at`].
#[derive(Debug)]
struct FixedObservationClock;

impl Clock for FixedObservationClock {
    fn now(&self) -> tokio::time::Instant {
        tokio::time::Instant::now()
    }

    fn wall_now(&self) -> SystemTime {
        UNIX_EPOCH + Duration::new(1_725_000_000, 123)
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> ClockSleep {
        Box::pin(tokio::time::sleep_until(deadline))
    }
}

struct BlockingCollector {
    snapshot: HardwareSnapshot,
    probe: CollectorProbe,
    release: Mutex<mpsc::Receiver<()>>,
}

impl BlockingCollector {
    fn new(snapshot: HardwareSnapshot) -> (Self, CollectorProbe, mpsc::Sender<()>) {
        let probe = CollectorProbe {
            calls: Arc::new(AtomicUsize::new(0)),
            called: Arc::new(Notify::new()),
        };
        let (release, receiver) = mpsc::channel();
        (
            Self {
                snapshot,
                probe: probe.clone(),
                release: Mutex::new(receiver),
            },
            probe,
            release,
        )
    }
}

impl HardwareSnapshotSource for BlockingCollector {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        self.probe.called.notify_waiters();
        self.release
            .lock()
            .expect("release receiver")
            .recv()
            .expect("test releases collection");
        self.snapshot.clone()
    }

    /// Samples without blocking or counting, so per-second sampling never affects the
    /// collection this fake controls.
    fn sample(&self) -> HardwareSample {
        HardwareSample::from_snapshot(&self.snapshot)
    }
}

struct CountingCollector {
    snapshot: HardwareSnapshot,
    probe: CollectorProbe,
}

#[derive(Clone)]
struct CollectorProbe {
    calls: Arc<AtomicUsize>,
    called: Arc<Notify>,
}

impl CountingCollector {
    fn new(snapshot: HardwareSnapshot) -> (Self, CollectorProbe) {
        let probe = CollectorProbe {
            calls: Arc::new(AtomicUsize::new(0)),
            called: Arc::new(Notify::new()),
        };
        (
            Self {
                snapshot,
                probe: probe.clone(),
            },
            probe,
        )
    }
}

impl CollectorProbe {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn wait_for_calls(&self, count: usize) {
        loop {
            let notified = self.called.notified();
            if self.calls() >= count {
                return;
            }
            notified.await;
        }
    }
}

impl HardwareSnapshotSource for CountingCollector {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.probe.calls.fetch_add(1, Ordering::SeqCst);
        self.probe.called.notify_waiters();
        self.snapshot.clone()
    }

    /// Samples without counting, so per-second sampling never shows as a collection.
    fn sample(&self) -> HardwareSample {
        HardwareSample::from_snapshot(&self.snapshot)
    }
}

fn local_snapshot() -> HardwareSnapshot {
    HardwareSnapshot {
        observed_at: Some(fixed_observed_at()),
        host: Some(HostObservation {
            result: Some(host_observation::Result::Facts(local_host_facts())),
        }),
        accelerators: vec![AcceleratorObservation {
            vendor: AcceleratorVendor::Nvidia as i32,
            result: Some(accelerator_observation::Result::Facts(
                AcceleratorFactsSet {
                    facts: local_accelerator_facts(),
                },
            )),
        }],
    }
}

fn assign_fixture_target_to_worker(root: &std::path::Path) {
    support::install_manager_trust(root);
    let site_path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
    site.nodes.push(SiteNode {
        node_id: "worker-node".to_owned(),
        host: "127.0.0.2".to_owned(),
        display_name: "Worker node".to_owned(),
        ..Default::default()
    });
    site.execution_targets[0].site_node_id = "worker-node".to_owned();
    site.model_deployments[0]
        .placement
        .as_mut()
        .expect("model exact placement")
        .site_node_id = "worker-node".to_owned();
    std::fs::write(
        site_path,
        dotcore_proto::yaml::to_string(&site).expect("two-node site YAML"),
    )
    .expect("two-node site fixture");
}

fn fixed_observed_at() -> Timestamp {
    Timestamp {
        seconds: 1_725_000_000,
        nanos: 123,
    }
}

fn local_host_facts() -> HostFacts {
    HostFacts {
        host_name: "fixture-node".to_owned(),
        operating_system_or_kernel: "NixOS 26.05".to_owned(),
        cpu_count: 16,
        total_memory_bytes: 68_719_476_736,
        available_memory_bytes: 34_359_738_368,
        cached_memory_bytes: None,
        filesystems: vec![dotcore_proto::dotcore::observation::v1::FilesystemFacts {
            mountpoint: "/".to_owned(),
            fstype: "ext4".to_owned(),
            size_bytes: 1_099_511_627_776,
        }],
    }
}

fn local_accelerator_facts() -> Vec<AcceleratorFacts> {
    vec![AcceleratorFacts {
        vendor: AcceleratorVendor::Nvidia as i32,
        model: "Fixture GPU".to_owned(),
        memory_architecture:
            dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated as i32,
        total_memory_bytes: Some(24_000),
        driver_or_runtime_version: "570.42.01".to_owned(),
        device_id: "GPU-fixture".to_owned(),
        vendor_index: Some(0),
        used_memory_bytes: Some(6_000),
        working_set_limit_bytes: None,
        supported_signals: Vec::new(),
    }]
}

fn configure_remote_node(root: &std::path::Path) {
    support::install_manager_trust(root);
    let site_path = root.join("site.yaml");
    let mut site: Site = dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
    site.nodes.push(SiteNode {
        node_id: "remote-node".to_owned(),
        host: "127.0.0.2".to_owned(),
        display_name: "Remote node".to_owned(),
        ..Default::default()
    });
    std::fs::write(
        site_path,
        dotcore_proto::yaml::to_string(&site).expect("manager site YAML"),
    )
    .expect("manager site fixture");
}
