//! Public OpenAI API compatibility evidence over the test router.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

use dotcore::composition::model_service_for;
use dotcore_proto::dotcore::{
    catalog::v1::{UpstreamModelBoundary, UpstreamModelDataEgressClass, UpstreamModelLocality},
    inference::v1::{
        CreateRouteRequest, FallbackGroup, FallbackGroupSelection, ListRoutesRequest,
        ReplaceRouteRequest,
    },
    model::v1::{
        CreateModelDeploymentRequest, CreateUpstreamModelDeploymentRequest, DirectRouteExposure,
        ModelDeploymentExposure, OpenAiCompatibleEndpointConfiguration,
        UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
        model_deployment_exposure,
        model_deployment_service_server::ModelDeploymentService as ModelServiceContract,
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
    task::JoinHandle,
};
use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};

use crate::support;

const PRIVATE_FIELD_NAMES: &[&str] = &[
    "deployment_id",
    "runtime_id",
    "container_id",
    "engine_address",
    "host_port",
    "reconcile_generation",
];

struct FixtureRouter {
    _root: support::TemporaryFixture,
    address: SocketAddr,
    management: tonic::transport::Channel,
    shutdown: oneshot::Sender<()>,
    server: JoinHandle<dotcore::composition::CompositionResult<()>>,
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
}

#[tokio::test]
async fn model_list_has_the_public_openai_shape() {
    let router = fixture_router().await;
    let response = router
        .request("GET", "/v1/models", b"")
        .await
        .expect("Model list request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    assert_eq!(
        json_body(&response),
        serde_json::json!({
            "object": "list",
            "data": [{
                "id": "fixture-route",
                "object": "model",
                "created": 0,
                "owned_by": "dotcore",
                "context_length": 32768
            }]
        })
    );

    router.stop().await;
}

#[tokio::test]
async fn route_discovery_lists_only_enabled_route_names_in_sorted_order() {
    let router = direct_route_fixture_router("direct-route").await;
    let mut routes = router.route_client();
    let direct = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list exposure route")
        .into_inner()
        .routes
        .into_iter()
        .next()
        .expect("exposure route view")
        .route
        .expect("exposure route declaration");
    let deployment_id = direct
        .fallback_groups
        .first()
        .expect("Direct fallback group")
        .model_deployment_ids
        .first()
        .expect("Direct deployment")
        .clone();
    for name in ["zulu-route", "alpha-route"] {
        routes
            .create_route(CreateRouteRequest {
                name: name.to_owned(),
                enabled: true,
                fallback_groups: vec![FallbackGroup {
                    selection: FallbackGroupSelection::Single as i32,
                    model_deployment_ids: vec![deployment_id.clone()],
                }],
            })
            .await
            .expect("create enabled route");
    }
    let disabled = routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: direct.inference_route_id,
            expected_generation: direct.generation,
            name: "disabled-route".to_owned(),
            enabled: false,
            fallback_groups: direct.fallback_groups,
        })
        .await
        .expect("disable exposure route")
        .into_inner()
        .route
        .expect("disabled route")
        .route
        .expect("disabled route declaration");
    assert_eq!(disabled.name, "disabled-route");

    let response = router
        .request("GET", "/v1/models", b"")
        .await
        .expect("route discovery request");

    assert_eq!(response.status, 200);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"object":"list","data":[
            {"id":"alpha-route","object":"model","created":0,"owned_by":"dotcore","context_length":32768},
            {"id":"zulu-route","object":"model","created":0,"owned_by":"dotcore","context_length":32768}
        ]}),
        "discovery must sort enabled route names and omit disabled routes and raw catalog identities"
    );

    router.stop().await;
}

#[tokio::test]
async fn direct_route_admission_echoes_the_route_name_and_rejects_the_catalog_model_id() {
    let router = direct_route_fixture_router("direct-route").await;
    let response = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"direct-route","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("exposure route request");

    assert_eq!(response.status, 200);
    assert_eq!(json_body(&response)["model"], "direct-route");

    let raw_model = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("raw catalog model request");
    assert_not_found_route(&raw_model);

    router.stop().await;
}

#[tokio::test]
async fn a_route_rename_applies_to_new_admissions_without_revoking_existing_targets() {
    let router = direct_route_fixture_router("before-rename").await;
    let mut routes = router.route_client();
    let route = routes
        .list_routes(ListRoutesRequest {})
        .await
        .expect("list exposure route")
        .into_inner()
        .routes
        .into_iter()
        .next()
        .expect("exposure route view")
        .route
        .expect("exposure route declaration");
    routes
        .replace_route(ReplaceRouteRequest {
            inference_route_id: route.inference_route_id,
            expected_generation: route.generation,
            name: "after-rename".to_owned(),
            enabled: true,
            fallback_groups: route.fallback_groups,
        })
        .await
        .expect("rename exposure route");

    let renamed = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"after-rename","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("renamed route request");
    assert_eq!(renamed.status, 200);
    assert_eq!(json_body(&renamed)["model"], "after-rename");

    let replaced = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"before-rename","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("replaced route request");
    assert_not_found_route(&replaced);

    router.stop().await;
}

#[tokio::test]
async fn an_enabled_route_without_a_ready_target_is_temporarily_unavailable() {
    let router = direct_route_fixture_router("pausable-route").await;
    let deployment_id = router
        .model_client()
        .list_model_deployments(dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {})
        .await
        .expect("list deployments")
        .into_inner()
        .deployments
        .into_iter()
        .next()
        .expect("Direct deployment")
        .model_deployment_id;
    router
        .model_client()
        .pause_model(dotcore_proto::dotcore::model::v1::PauseModelRequest {
            model_deployment_id: deployment_id,
        })
        .await
        .expect("pause deployment");

    let response = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"pausable-route","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("unavailable route request");

    assert_eq!(response.status, 503);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"error":{"message":"The requested model route has no ready serving target.","type":"server_error","param":null,"code":"model_not_ready"}})
    );

    router.stop().await;
}

#[tokio::test]
async fn system_one_decisions_have_the_public_shape_on_a_decision_route() {
    let router = decision_fixture_router("decision-route").await;

    let models = router
        .request("GET", "/v1/models", b"")
        .await
        .expect("model list");
    let response = router
        .request("POST", "/v1/systemone", SYSTEM_ONE_REQUEST.as_bytes())
        .await
        .expect("decision request");

    assert_eq!(
        json_body(&models)["data"],
        serde_json::json!([{"id":"decision-route","object":"model","created":0,"owned_by":"dotcore","context_length":32768}])
    );
    assert_eq!(response.status, 200);
    let body = String::from_utf8(response.body.clone()).expect("UTF-8 body");
    let positions = ["\"urgent\"", "\"category\"", "\"severity\""]
        .map(|name| body.find(name).expect("answer present"));
    assert!(
        positions.is_sorted(),
        "answers follow request order: {body}"
    );
    // The simulated engine derives probabilities from hashed weights; a tie selects the first.
    assert_eq!(
        json_body(&response),
        serde_json::json!({
            "model": "decision-route",
            "answers": {
                "urgent": {"type": "noul", "noul": 2.0 / 3.0},
                "category": {
                    "type": "choice",
                    "choice": "refund",
                    "probabilities": {"refund": 0.5, "other": 0.5},
                    "confidence": 0.0
                },
                "severity": {
                    "type": "score",
                    "score": 1.222222222222222,
                    "legend": {"0": "Minor", "1": "Major", "2": "Blocking"},
                    "probabilities": {"0": 2.0 / 9.0, "1": 3.0 / 9.0, "2": 4.0 / 9.0},
                    "confidence": 0.16666666666666663
                }
            },
            "usage": {"input_tokens": 10, "output_tokens": 0}
        })
    );

    router.stop().await;
}

#[tokio::test]
async fn system_one_rejects_invalid_requests_and_unserved_kinds_with_openai_envelopes() {
    let router = decision_fixture_router("decision-route").await;
    let error = |status: u16, kind: &str, code: &str, message: &str, param: Option<&str>| {
        (
            status,
            serde_json::json!({"error":{"message":message,"type":kind,"param":param,"code":code}}),
        )
    };
    let invalid = |param: &str| {
        error(
            400,
            "invalid_request_error",
            "invalid_value",
            &format!("Request field {param} is invalid."),
            Some(param),
        )
    };
    let unsupported = |param: Option<&str>| {
        error(
            400,
            "unsupported_feature",
            "unsupported_parameter",
            &param.map_or_else(
                || "The request asks for an unsupported feature.".to_owned(),
                |param| format!("Request field {param} is not supported."),
            ),
            param,
        )
    };
    let with = |change: &dyn Fn(&mut serde_json::Value)| {
        let mut value =
            serde_json::from_str::<serde_json::Value>(SYSTEM_ONE_REQUEST).expect("request JSON");
        change(&mut value);
        serde_json::to_vec(&value).expect("request bytes")
    };
    let cases = vec![
        (
            "missing model",
            "/v1/systemone",
            with(&|value| {
                value.as_object_mut().expect("object").remove("model");
            }),
            invalid("model"),
        ),
        (
            "missing state",
            "/v1/systemone",
            with(&|value| {
                value.as_object_mut().expect("object").remove("state");
            }),
            invalid("state"),
        ),
        (
            "images",
            "/v1/systemone",
            with(&|value| value["images"] = serde_json::json!(["aGk="])),
            unsupported(Some("images")),
        ),
        (
            "stream",
            "/v1/systemone",
            with(&|value| value["stream"] = serde_json::json!(true)),
            unsupported(Some("stream")),
        ),
        (
            "unknown field",
            "/v1/systemone",
            with(&|value| value["assets"] = serde_json::json!([])),
            unsupported(None),
        ),
        (
            "unknown question type",
            "/v1/systemone",
            with(&|value| value["questions"]["urgent"]["type"] = serde_json::json!("box")),
            unsupported(Some("type")),
        ),
        (
            "duplicate question",
            "/v1/systemone",
            br#"{"model":"decision-route","state":"s","questions":{"q":{"type":"noul","instructions":"a"},"q":{"type":"noul","instructions":"b"}}}"#.to_vec(),
            invalid("questions"),
        ),
        (
            "too many score levels",
            "/v1/systemone",
            with(&|value| {
                value["questions"]["severity"]["criteria"] =
                    serde_json::json!(["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11"]);
            }),
            invalid("criteria"),
        ),
        (
            "chat on a decision-only route",
            "/v1/chat/completions",
            br#"{"model":"decision-route","messages":[{"role":"user","content":"hello"}]}"#.to_vec(),
            error(
                400,
                "invalid_request_error",
                "unsupported_request_kind",
                "The requested model route does not serve this kind of request.",
                Some("model"),
            ),
        ),
    ];

    for (case, path, body, (status, expected)) in cases {
        let response = router
            .request("POST", path, &body)
            .await
            .expect("rejected request");
        assert_eq!(
            (response.status, json_body(&response)),
            (status, expected),
            "{case}"
        );
    }

    router.stop().await;
}

#[tokio::test]
async fn a_decision_route_without_a_ready_decision_member_is_unavailable() {
    let router = decision_fixture_router("decision-route").await;
    let deployment_id = router
        .model_client()
        .list_model_deployments(dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {})
        .await
        .expect("list deployments")
        .into_inner()
        .deployments
        .into_iter()
        .next()
        .expect("direct deployment")
        .model_deployment_id;
    router
        .model_client()
        .pause_model(dotcore_proto::dotcore::model::v1::PauseModelRequest {
            model_deployment_id: deployment_id,
        })
        .await
        .expect("pause deployment");

    let response = router
        .request("POST", "/v1/systemone", SYSTEM_ONE_REQUEST.as_bytes())
        .await
        .expect("unavailable route request");

    assert_eq!(response.status, 503);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"error":{"message":"The requested model route has no ready serving target.","type":"server_error","param":null,"code":"model_not_ready"}})
    );

    router.stop().await;
}

#[tokio::test]
async fn a_decision_on_a_chat_only_route_is_invalid() {
    let router = fixture_router().await;
    let request = SYSTEM_ONE_REQUEST.replace("decision-route", "fixture-route");

    let response = router
        .request("POST", "/v1/systemone", request.as_bytes())
        .await
        .expect("decision request");

    assert_eq!(response.status, 400);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"error":{"message":"The requested model route does not serve this kind of request.","type":"invalid_request_error","param":"model","code":"unsupported_request_kind"}})
    );

    router.stop().await;
}

#[tokio::test]
async fn image_requests_select_only_image_capable_route_members() {
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAAD0lEQVR4AQEEAPv/AAAAAAAEAAFlScNgAAAAAElFTkSuQmCC";
    let router = direct_route_fixture_router_with_image("image-route").await;
    let response = router
        .request(
            "POST",
            "/v1/chat/completions",
            &serde_json::to_vec(&serde_json::json!({"model":"image-route","messages":[{"role":"user","content":[{"type":"text","text":"Describe this."},{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}]})).expect("image request"),
        )
        .await
        .expect("image route request");

    assert_eq!(response.status, 200);
    assert_eq!(json_body(&response)["model"], "image-route");

    router.stop().await;
}

#[tokio::test]
async fn synchronous_chat_has_the_public_openai_shape() {
    let router = fixture_router().await;
    let response = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"hello"}]}"#,
        )
        .await
        .expect("synchronous Chat request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    assert_eq!(
        json_body(&response),
        serde_json::json!({
            "id": "chatcmpl_simulated",
            "object": "chat.completion",
            "created": 0,
            "model": "fixture-route",
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
    );

    router.stop().await;
}

#[tokio::test]
async fn streamed_chat_has_terminal_done_and_streamed_tool_arguments() {
    let router = fixture_router().await;
    let response = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"use a tool"}],"tools":[{"type":"function","function":{"name":"read_file","parameters":{"type":"object"}}}],"stream":true,"stream_options":{"include_usage":true}}"#,
        )
        .await
        .expect("streamed Chat request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    let text = std::str::from_utf8(&response.body).expect("SSE response is UTF-8");
    assert!(text.ends_with("data: [DONE]\n\n"));
    let events = sse_events(text);
    assert_eq!(events.len(), 5);
    assert_eq!(events[0]["object"], "chat.completion.chunk");
    assert_eq!(events[0]["id"], "chatcmpl_simulated");
    assert_eq!(events[0]["model"], "fixture-route");
    assert_eq!(
        events[1]["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_simulated_0"
    );
    assert_eq!(
        events[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert_eq!(
        events[1]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "{"
    );
    assert_eq!(
        events[2]["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
        "}"
    );
    assert_eq!(events[3]["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(
        events[4]["usage"],
        serde_json::json!({
            "prompt_tokens": 1,
            "completion_tokens": 1,
            "total_tokens": 2
        })
    );

    let follow_up = router
        .request(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"use a tool"},{"role":"assistant","content":null,"tool_calls":[{"id":"call_simulated_0","type":"function","function":{"name":"read_file","arguments":"{}"}}]},{"role":"tool","tool_call_id":"call_simulated_0","content":"file contents"}]}"#,
        )
        .await
        .expect("tool-result Chat request");
    assert_eq!(follow_up.status, 200);
    assert_public_fields_absent(&follow_up.body);
    assert_eq!(
        json_body(&follow_up)["choices"][0]["message"]["content"],
        "Simulated response."
    );

    router.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_cancellation_closes_public_sse_without_done() {
    let router = fixture_router().await;
    // The oversized tool name fills the next SSE write and gives cancellation a controlled point.
    let tool_name = "tool".to_owned() + &"x".repeat(900_000);
    let request = serde_json::to_vec(&serde_json::json!({
        "model": "fixture-route",
        "messages": [{"role": "user", "content": "use a tool"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": tool_name,
                "parameters": {"type": "object"}
            }
        }],
        "stream": true
    }))
    .expect("delayed stream request");
    let mut stream = router
        .start_request("POST", "/v1/chat/completions", &request)
        .await
        .expect("stream request");
    let mut response = read_through_first_sse_frame(&mut stream).await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("SSE response headers");
    let first_frame = &response[header_end + 4..];
    assert!(first_frame.starts_with(b"data: "));
    assert!(
        first_frame
            .windows(b"\"choices\":[]".len())
            .any(|window| window == b"\"choices\":[]")
    );

    stream.shutdown().await.expect("client SSE cancellation");
    tokio::time::timeout(
        support::CONVERGENCE_DEADLINE,
        stream.read_to_end(&mut response),
    )
    .await
    .expect("cancelled SSE close timeout")
    .expect("cancelled SSE close");
    assert!(!String::from_utf8_lossy(&response).contains("data: [DONE]"));

    router.stop().await;
}

#[tokio::test]
async fn errors_use_openai_error_envelopes() {
    let router = fixture_router().await;
    for (method, path, body, status, error_type, message) in [
        (
            "GET",
            "/v1/responses",
            b"".as_slice(),
            400,
            "unsupported_endpoint",
            "This OpenAI endpoint is not supported.",
        ),
        (
            "POST",
            "/v1/embeddings",
            br#"{}"#.as_slice(),
            400,
            "unsupported_endpoint",
            "This OpenAI endpoint is not supported.",
        ),
        (
            "POST",
            "/v1/chat/completions",
            br#"{"messages":[]}"#.as_slice(),
            400,
            "invalid_request_error",
            "model is required.",
        ),
        (
            "GET",
            "/v1/unknown",
            b"".as_slice(),
            404,
            "invalid_request_error",
            "The requested endpoint does not exist.",
        ),
    ] {
        let response = router
            .request(method, path, body)
            .await
            .expect("error request");
        assert_eq!(response.status, status, "{method} {path}");
        assert_public_fields_absent(&response.body);
        assert_eq!(
            json_body(&response),
            serde_json::json!({
                "error": {
                    "message": message,
                    "type": error_type,
                    "param": null,
                    "code": null
                }
            })
        );
    }

    router.stop().await;
}

#[tokio::test]
async fn public_responses_do_not_expose_private_router_fields() {
    let router = fixture_router().await;
    for (method, path, body) in [
        (
            "GET",
            "/v1/models",
            b"".as_slice(),
        ),
        (
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"hello"}]}"#.as_slice(),
        ),
        (
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"hello"}],"stream":true}"#.as_slice(),
        ),
        (
            "POST",
            "/v1/responses",
            br#"{"model":"fixture-route","input":"hello","store":false}"#.as_slice(),
        ),
    ] {
        let response = router
            .request(method, path, body)
            .await
            .expect("public response request");
        assert_eq!(response.status, 200, "{method} {path}");
        assert_public_fields_absent(&response.body);
    }
    router.stop().await;
}

#[tokio::test]
async fn synchronous_responses_support_full_text_and_function_history() {
    let router = fixture_router().await;
    let response = router
        .request(
            "POST",
            "/v1/responses",
            br#"{"model":"fixture-route","store":false,"instructions":"Be concise.","input":[{"type":"message","role":"user","content":"Read the file."},{"type":"message","id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"I will read it.","annotations":[]}]},{"type":"function_call","id":"fc_call_1","status":"completed","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"},{"type":"function_call_output","id":"fco_1","status":"completed","call_id":"call_1","output":"file contents"},{"type":"message","role":"user","content":[{"type":"input_text","text":"Summarize it."}]}],"tools":[{"type":"function","name":"read_file","description":"Read a file.","parameters":{"type":"object"},"strict":false}],"tool_choice":"auto","parallel_tool_calls":true,"max_output_tokens":64,"temperature":0.25,"top_p":0.75}"#,
        )
        .await
        .expect("Responses request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    let response = json_body(&response);
    assert_eq!(response["object"], "response");
    assert_eq!(response["status"], "completed");
    assert_eq!(response["model"], "fixture-route");
    assert_eq!(response["store"], false);
    assert_eq!(response["output"][0]["type"], "message");
    assert_eq!(
        response["output"][0]["content"][0]["text"],
        "Simulated response."
    );
    assert_eq!(
        response["usage"],
        serde_json::json!({"input_tokens":6,"output_tokens":3,"total_tokens":9})
    );

    router.stop().await;
}

#[tokio::test]
async fn responses_accept_opaque_prompt_cache_keys_and_bare_message_items() {
    let router = fixture_router().await;
    let response = router
        .request(
            "POST",
            "/v1/responses",
            br#"{"model":"fixture-route","store":false,"prompt_cache_key":"session_123","input":[{"role":"system","content":"Be concise."},{"role":"developer","content":"Use plain text."},{"role":"user","content":[{"type":"input_text","text":"Hello."}]},{"id":"msg_1","status":"completed","role":"assistant","content":[{"type":"output_text","text":"Prior reply.","annotations":[]}]}]}"#,
        )
        .await
        .expect("Responses request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    let response = json_body(&response);
    assert_eq!(response["output"][0]["type"], "message");
    assert_eq!(response["usage"]["input_tokens"], 4);

    router.stop().await;
}

#[tokio::test]
async fn streamed_responses_emit_ordered_typed_function_events_without_chat_done() {
    let router = fixture_router().await;
    let response = router
        .request(
            "POST",
            "/v1/responses",
            br#"{"model":"fixture-route","store":false,"input":"use a tool","tools":[{"type":"function","name":"read_file","parameters":{"type":"object"}}],"parallel_tool_calls":true,"stream":true}"#,
        )
        .await
        .expect("streamed Responses request");

    assert_eq!(response.status, 200);
    assert_public_fields_absent(&response.body);
    let text = std::str::from_utf8(&response.body).expect("Responses SSE is UTF-8");
    assert!(!text.contains("[DONE]"));
    let events = responses_sse_events(text);
    assert_eq!(
        events
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        [
            "response.created",
            "response.in_progress",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed",
        ]
    );
    assert_eq!(events[2].1["item"]["type"], "function_call");
    assert_eq!(events[3].1["delta"], "{");
    assert_eq!(events[4].1["delta"], "}");
    assert_eq!(events[7].1["response"]["output"][0]["arguments"], "{}");

    router.stop().await;
}

#[tokio::test]
async fn responses_reject_deferred_capabilities_with_safe_public_errors() {
    let router = fixture_router().await;
    for body in [
        br#"{"model":"fixture-route","input":"hello"}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":true}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"previous_response_id":"resp_1"}"#.as_slice(),
        br#"{"model":"fixture-route","input":[{"type":"item_reference","id":"msg_1"}],"store":false}"#.as_slice(),
        br#"{"model":"fixture-route","input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,aQ=="}]}],"store":false}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"tools":[{"type":"web_search_preview"}]}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"reasoning":{"effort":"low","generate_summary":"auto"}}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"include":["message.output_text.logprobs"]}"#.as_slice(),
        br#"{"model":"fixture-route","input":[{"type":"reasoning","summary":[],"encrypted_content":"gAAAA"}],"store":false}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"text":{"format":{"type":"json_schema"}}}"#.as_slice(),
        br#"{"model":"fixture-route","input":"hello","store":false,"conversation":"conv_1"}"#.as_slice(),
    ] {
        let response = router
            .request("POST", "/v1/responses", body)
            .await
            .expect("deferred Responses request");
        assert_eq!(response.status, 400);
        assert_public_fields_absent(&response.body);
        assert!(json_body(&response)["error"]["message"].is_string());
    }
    for (method, path) in [
        ("GET", "/v1/responses/resp_1"),
        ("DELETE", "/v1/responses/resp_1"),
        ("POST", "/v1/conversations"),
    ] {
        let response = router
            .request(method, path, b"")
            .await
            .expect("deferred Responses endpoint request");
        assert_eq!(response.status, 404);
        assert_public_fields_absent(&response.body);
        assert_eq!(
            json_body(&response)["error"]["type"],
            "invalid_request_error"
        );
    }
    router.stop().await;
}

#[tokio::test]
async fn image_content_routes_only_to_a_ready_image_capable_snapshot() {
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAAD0lEQVR4AQEEAPv/AAAAAAAEAAFlScNgAAAAAElFTkSuQmCC";
    let router = image_fixture_router().await;
    for (path, body) in [
        (
            "/v1/chat/completions",
            serde_json::json!({"model":"fixture-route","messages":[{"role":"user","content":[{"type":"text","text":"Describe this."},{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}]}),
        ),
        (
            "/v1/responses",
            serde_json::json!({"model":"fixture-route","store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"Describe this."},{"type":"input_image","image_url":image,"detail":"auto"}]}]}),
        ),
    ] {
        let response = router
            .request(
                "POST",
                path,
                &serde_json::to_vec(&body).expect("image request"),
            )
            .await
            .expect("image request");
        assert_eq!(response.status, 200, "{path}");
        assert_public_fields_absent(&response.body);
    }
    for (image_url, detail, expected) in [
        (
            "https://example.test/image.png",
            "auto",
            "URL has no data URL metadata separator",
        ),
        (
            "file:///tmp/image.png",
            "auto",
            "URL has no data URL metadata separator",
        ),
        (
            "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJ",
            "auto",
            "URL has no data URL metadata separator",
        ),
        (
            "data:image/png;base64,/9j/4AAQ",
            "auto",
            "Png MIME type does not match payload magic bytes",
        ),
        (
            "data:image/png;base64,iVBORw0KGgo=",
            "auto",
            "Png payload has no terminal marker",
        ),
        (image, "low", "detail \"low\" must be \"auto\""),
    ] {
        let response = router
            .request(
                "POST",
                "/v1/chat/completions",
                &serde_json::to_vec(&serde_json::json!({"model":"fixture-route","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":image_url,"detail":detail}}]}]})).expect("invalid image request"),
            )
            .await
            .expect("invalid image response");
        assert_eq!(response.status, 400);
        assert_public_fields_absent(&response.body);
        assert_image_error_names(&response, expected);
    }
    // Capacity and unsupported-media rejections name the selected model's limit too.
    for (content, expected) in [
        (
            serde_json::json!([
                {"type":"image_url","image_url":{"url":image,"detail":"auto"}},
                {"type":"image_url","image_url":{"url":image,"detail":"auto"}}
            ]),
            "request has 2 images; maximum is 1",
        ),
        (
            serde_json::json!([{"type":"image_url","image_url":{"url":"data:image/jpeg;base64,/9j/2Q==","detail":"auto"}}]),
            "media type Jpeg is not in the selected model supported media types",
        ),
    ] {
        let response = router
            .request(
                "POST",
                "/v1/chat/completions",
                &serde_json::to_vec(&serde_json::json!({"model":"fixture-route","messages":[{"role":"user","content":content}]})).expect("image capacity request"),
            )
            .await
            .expect("image capacity response");
        assert_eq!(response.status, 400);
        assert_public_fields_absent(&response.body);
        assert_image_error_names(&response, expected);
    }
    // The Responses endpoint projects the same actionable cause.
    let response = router
        .request(
            "POST",
            "/v1/responses",
            &serde_json::to_vec(&serde_json::json!({"model":"fixture-route","store":false,"input":[{"type":"message","role":"user","content":[{"type":"input_image","image_url":"https://example.test/image.png"}]}]})).expect("invalid Responses image request"),
        )
        .await
        .expect("invalid Responses image response");
    assert_eq!(response.status, 400);
    assert_image_error_names(&response, "URL has no data URL metadata separator");
    router.stop().await;

    let router = fixture_router().await;
    let response = router.request("POST", "/v1/chat/completions", &serde_json::to_vec(&serde_json::json!({"model":"fixture-route","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}]})).expect("image request")).await.expect("image rejection");
    assert_eq!(response.status, 503);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"error":{"message":"The requested model route has no ready serving target.","type":"server_error","param":null,"code":"model_not_ready"}})
    );
    router.stop().await;
}

#[tokio::test]
async fn oversized_declared_request_body_returns_a_public_413() {
    let router = fixture_router().await;
    let mut stream = tokio::net::TcpStream::connect(router.address)
        .await
        .expect("router connection");
    stream
        .write_all(b"POST /v1/chat/completions HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n")
        .await
        .expect("oversized request headers");
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .await
        .expect("oversized response");
    let response = parse_response(&response).expect("oversized HTTP response");
    assert_eq!(response.status, 413);
    assert_eq!(
        json_body(&response)["error"]["type"],
        "invalid_request_error"
    );
    assert_public_fields_absent(&response.body);
    router.stop().await;
}

#[test]
#[ignore = "Deferred embeddings support should provide an OpenAI API-compatible embeddings endpoint with vectors and usage. It is not supported now."]
fn embeddings_deferred_compatibility_stub() {
    todo!(
        "Deferred embeddings support should provide an OpenAI API-compatible embeddings endpoint with vectors and usage"
    );
}

#[test]
#[ignore = "Deferred hosted-tools support should accept OpenAI-hosted web, file, code-interpreter, and computer tools. It is not supported now."]
fn hosted_tools_deferred_compatibility_stub() {
    todo!(
        "Deferred hosted-tools support should accept OpenAI-hosted web, file, code-interpreter, and computer tools"
    );
}

#[test]
#[ignore = "Deferred structured-output support should accept a response schema and return schema-conforming output. It is not supported now."]
fn structured_output_deferred_compatibility_stub() {
    todo!(
        "Deferred structured-output support should accept a response schema and return schema-conforming output"
    );
}

#[tokio::test]
async fn engine_controls_and_outputs_are_faithful_through_the_public_boundary() {
    let engine = EngineRouter::start("faithful-route").await;

    // A Pi-shaped streamed request with ninfer sampling and effort controls.
    engine.script(200, "text/event-stream", REASONING_TOOL_STREAM.as_bytes());
    let pi_request =
        PI_CHAT_REQUEST.replace("\"model\":\"qwen3-8-27b\"", "\"model\":\"faithful-route\"");
    let response = engine
        .request("/v1/chat/completions", pi_request.as_bytes())
        .await;
    assert_eq!(response.status, 200);
    let sent = engine.last_request();
    assert!(sent.contains(r#""seed":18446744073709551615"#));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&sent).expect("engine request JSON"),
        serde_json::json!({
            "model": ENGINE_MODEL,
            "messages": [
                {"role": "developer", "content": "You are Pi."},
                {"role": "user", "content": "Read the README."},
                {
                    "role": "assistant",
                    "content": null,
                    "reasoning": "The file is needed.",
                    "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"path\":\"README.md\"}"}}]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "# Readme"}
            ],
            "tools": [{"type": "function", "function": {"name": "read", "description": "Read a file.", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}, "strict": false}}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "store": false,
            "max_completion_tokens": 32000,
            "reasoning_effort": "none",
            "temperature": 1.0_f32,
            "top_p": 0.95_f32,
            "presence_penalty": 0.0,
            "frequency_penalty": 0.0,
            "top_k": 20,
            "repetition_penalty": 1.0,
            "min_p": 0,
            "seed": 18446744073709551615_u64,
            "chat_template_kwargs": {"enable_thinking": false, "preserve_thinking": true}
        })
    );
    let body = String::from_utf8(response.body).expect("public SSE");
    assert!(body.ends_with("data: [DONE]\n\n"));
    let chunks = sse_events(&body);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk["model"] == "faithful-route")
    );
    let deltas = chunks
        .iter()
        .flat_map(|chunk| chunk["choices"].as_array().cloned().unwrap_or_default())
        .collect::<Vec<_>>();
    let reasoning = deltas
        .iter()
        .filter_map(|choice| choice["delta"]["reasoning"].as_str())
        .collect::<String>();
    assert_eq!(reasoning, "Let me check.");
    assert!(
        deltas.iter().all(|choice| !choice["delta"]["content"]
            .as_str()
            .is_some_and(|text| !text.is_empty())),
        "reasoning never becomes final content"
    );
    let tool_calls = deltas
        .iter()
        .filter_map(|choice| choice["delta"]["tool_calls"].as_array())
        .flatten()
        .map(|call| {
            (
                call["id"].clone(),
                call["function"]["name"].clone(),
                call["function"]["arguments"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tool_calls,
        [
            (
                serde_json::json!("call_1"),
                serde_json::json!("read"),
                serde_json::json!("")
            ),
            (
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::json!("{\"pa")
            ),
            (
                serde_json::Value::Null,
                serde_json::Value::Null,
                serde_json::json!("th\":\"README.md\"}")
            ),
        ]
    );
    assert_eq!(
        deltas
            .iter()
            .filter_map(|choice| choice["finish_reason"].as_str())
            .collect::<Vec<_>>(),
        ["tool_calls"]
    );
    assert_eq!(
        chunks.last().expect("usage chunk")["usage"],
        serde_json::json!({"prompt_tokens":30,"completion_tokens":12,"total_tokens":42,"prompt_tokens_details":{"cached_tokens":16},"completion_tokens_details":{"reasoning_tokens":5}})
    );

    // A two-turn vLLM-style conversation with template kwargs, reasoning history, and a tool result.
    engine.script(
        200,
        "application/json",
        br#"{"id":"c1","object":"chat.completion","created":3,"model":"engine-private-model","choices":[{"index":0,"message":{"role":"assistant","reasoning_content":"Need the weather.","tool_calls":[{"id":"call_w","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Oslo\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13,"completion_tokens_details":{"reasoning_tokens":3}}}"#,
    );
    let tools = r#""tools":[{"type":"function","function":{"name":"weather","parameters":{"type":"object"}}}]"#;
    let first_turn = format!(
        r#"{{"model":"faithful-route","messages":[{{"role":"system","content":"Be brief."}},{{"role":"user","content":"Weather in Oslo?"}}],{tools},"reasoning_effort":"high","chat_template_kwargs":{{"enable_thinking":true,"preserve_thinking":true}}}}"#
    );
    let response = engine
        .request("/v1/chat/completions", first_turn.as_bytes())
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        json_body(&response)["choices"][0],
        serde_json::json!({"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_w","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Oslo\"}"}}],"reasoning":"Need the weather."},"finish_reason":"tool_calls"})
    );
    assert_eq!(
        json_body(&response)["usage"],
        serde_json::json!({"prompt_tokens":9,"completion_tokens":4,"total_tokens":13,"completion_tokens_details":{"reasoning_tokens":3}})
    );
    let sent = serde_json::from_str::<serde_json::Value>(&engine.last_request()).expect("JSON");
    assert_eq!(sent["reasoning_effort"], "high");
    assert_eq!(
        sent["chat_template_kwargs"],
        serde_json::json!({"enable_thinking":true,"preserve_thinking":true})
    );

    engine.script(
        200,
        "application/json",
        br#"{"id":"c2","object":"chat.completion","created":4,"model":"engine-private-model","choices":[{"index":0,"message":{"role":"assistant","content":"Sunny.","reasoning_content":null},"finish_reason":"stop"}],"usage":null}"#,
    );
    let second_turn = format!(
        r#"{{"model":"faithful-route","messages":[{{"role":"user","content":"Weather in Oslo?"}},{{"role":"assistant","content":null,"reasoning_content":"Need the weather.","tool_calls":[{{"id":"call_w","type":"function","function":{{"name":"weather","arguments":"{{\"city\":\"Oslo\"}}"}}}}]}},{{"role":"tool","tool_call_id":"call_w","content":"sunny"}}],{tools}}}"#
    );
    let response = engine
        .request("/v1/chat/completions", second_turn.as_bytes())
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(
        json_body(&response)["choices"][0]["message"],
        serde_json::json!({"role":"assistant","content":"Sunny."})
    );
    assert!(
        json_body(&response).get("usage").is_none(),
        "null usage stays absent"
    );
    let sent = serde_json::from_str::<serde_json::Value>(&engine.last_request()).expect("JSON");
    assert_eq!(
        sent["messages"],
        serde_json::json!([
            {"role":"user","content":"Weather in Oslo?"},
            {"role":"assistant","content":null,"reasoning":"Need the weather.","tool_calls":[{"id":"call_w","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Oslo\"}"}}]},
            {"role":"tool","tool_call_id":"call_w","content":"sunny"}
        ])
    );

    // Text and image input.
    engine.script(200, "application/json", SIMPLE_COMPLETION.as_bytes());
    let image = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAAD0lEQVR4AQEEAPv/AAAAAAAEAAFlScNgAAAAAElFTkSuQmCC";
    let response = engine
        .request(
            "/v1/chat/completions",
            &serde_json::to_vec(&serde_json::json!({"model":"faithful-route","messages":[{"role":"user","content":[{"type":"text","text":"Describe this."},{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}],"temperature":0}))
                .expect("image request"),
        )
        .await;
    assert_eq!(response.status, 200);
    let sent = serde_json::from_str::<serde_json::Value>(&engine.last_request()).expect("JSON");
    assert_eq!(
        sent["messages"],
        serde_json::json!([{"role":"user","content":[{"type":"text","text":"Describe this."},{"type":"image_url","image_url":{"url":image,"detail":"auto"}}]}])
    );
    assert_eq!(
        sent["temperature"], 0.0,
        "a zero control is sent, not dropped"
    );

    // The limited Responses equivalent requests and reports real terminal usage.
    engine.script(200, "text/event-stream", NINFER_USAGE_STREAM.as_bytes());
    let response = engine
        .request(
            "/v1/responses",
            br#"{"model":"faithful-route","store":false,"instructions":"You are Pi.","input":"Hi","stream":true,"max_output_tokens":64,"reasoning":{"effort":"xhigh"},"top_k":20,"prompt_cache_key":"session-1"}"#,
        )
        .await;
    assert_eq!(response.status, 200);
    let sent = serde_json::from_str::<serde_json::Value>(&engine.last_request()).expect("JSON");
    assert_eq!(
        sent,
        serde_json::json!({
            "model": ENGINE_MODEL,
            "messages": [{"role":"system","content":"You are Pi."},{"role":"user","content":"Hi"}],
            "stream": true,
            "stream_options": {"include_usage": true},
            "store": false,
            "max_completion_tokens": 64,
            "reasoning_effort": "xhigh",
            "top_k": 20,
            "prompt_cache_key": "session-1"
        })
    );
    let events = responses_sse_events(&String::from_utf8(response.body).expect("Responses SSE"));
    let (name, completed) = events.last().expect("completed event");
    assert_eq!(name, "response.completed");
    assert_eq!(completed["response"]["model"], "faithful-route");
    assert_eq!(
        completed["response"]["usage"],
        serde_json::json!({"input_tokens":12,"output_tokens":2,"total_tokens":14,"input_tokens_details":{"cached_tokens":4},"output_tokens_details":{"reasoning_tokens":1}})
    );

    // An engine refusal of a forwarded control is a safe upstream request failure.
    engine.script(
        400,
        "application/json",
        br#"{"error":{"message":"top_k engine-body-sentinel","param":"top_k"}}"#,
    );
    let response = engine
        .request(
            "/v1/chat/completions",
            br#"{"model":"faithful-route","messages":[{"role":"user","content":"hi"}],"top_k":-7}"#,
        )
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(
        json_body(&response),
        serde_json::json!({"error":{"message":"The inference engine rejected the request.","type":"upstream_error","param":"top_k","code":null}})
    );

    engine.stop().await;
}

#[tokio::test]
async fn refused_controls_never_reach_the_engine_or_leak_their_values() {
    let engine = EngineRouter::start("refusal-route").await;
    let attempts = engine.chat_attempts();
    let mut client_errors = Vec::new();
    for (path, body, status, param) in [
        (
            "/v1/chat/completions",
            r#"{"model":"refusal-route","messages":[{"role":"user","content":"prompt-sentinel"}],"base_url":"http://extension-sentinel"}"#,
            400,
            serde_json::Value::Null,
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"refusal-route","messages":[{"role":"user","content":"prompt-sentinel"}],"sentinel-key":1,"model_override":2,"authorization":"Bearer extension-sentinel"}"#,
            400,
            serde_json::Value::Null,
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"refusal-route","messages":[{"role":"user","content":"prompt-sentinel"}],"response_format":{"type":"json_object"}}"#,
            400,
            serde_json::json!("response_format"),
        ),
        (
            "/v1/chat/completions",
            r#"{"model":"refusal-route","messages":[{"role":"user","content":"prompt-sentinel"}],"store":true}"#,
            400,
            serde_json::json!("store"),
        ),
        (
            "/v1/responses",
            r#"{"model":"refusal-route","store":false,"input":"prompt-sentinel","reasoning":{"effort":"low","generate_summary":"auto"}}"#,
            400,
            serde_json::json!("reasoning"),
        ),
        (
            "/v1/responses",
            r#"{"model":"refusal-route","store":false,"input":"prompt-sentinel","n":2}"#,
            400,
            serde_json::json!("n"),
        ),
    ] {
        let response = engine.request(path, body.as_bytes()).await;
        assert_eq!(response.status, status, "{body}");
        assert_eq!(json_body(&response)["error"]["param"], param, "{body}");
        let error = &json_body(&response)["error"];
        let text = |name: &str| error[name].as_str().unwrap_or("none").to_owned();
        client_errors.push(BTreeMap::from([
            ("status".to_owned(), status.to_string()),
            ("error_type".to_owned(), text("type")),
            ("code".to_owned(), text("code")),
            ("param".to_owned(), text("param")),
            ("error_message".to_owned(), text("message")),
        ]));
        let public = String::from_utf8_lossy(&response.body);
        for sentinel in ["sentinel", "http://", ENGINE_MODEL] {
            assert!(
                !public.contains(sentinel),
                "{sentinel} leaked into {public}"
            );
        }
    }
    assert_eq!(
        engine.chat_attempts(),
        attempts,
        "refused requests never reach the engine"
    );
    assert_eq!(
        rejected_request_events(),
        client_errors,
        "each refusal logs exactly the error its client received"
    );
    engine.assert_backend_logs_private(&["prompt-sentinel", "extension-sentinel", "sentinel-key"]);

    engine.stop().await;
}

#[tokio::test]
async fn engine_stream_failures_are_terminal_attributable_and_never_retried() {
    let engine = EngineRouter::start("failure-route").await;
    let request = br#"{"model":"failure-route","messages":[{"role":"user","content":"prompt-sentinel"}],"stream":true,"top_k":5}"#;

    // Invalid output before the public stream commits is an HTTP error.
    engine.script(200, "text/event-stream", b"data: {not json}\n\n");
    let attempts = engine.chat_attempts();
    let response = engine.request("/v1/chat/completions", request).await;
    assert_eq!(response.status, 502);
    assert_eq!(
        json_body(&response)["error"]["message"],
        "The inference engine returned an invalid response."
    );
    assert_eq!(engine.chat_attempts(), attempts + 1);

    // Invalid output after twenty valid tokens terminates the committed stream.
    let mut stream = String::new();
    for index in 0..20 {
        stream.push_str(&format!(
            "data: {{\"id\":\"c\",\"model\":\"engine-private-model\",\"created\":1,\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"token-{index} \"}},\"finish_reason\":null}}],\"usage\":null}}\n\n"
        ));
    }
    stream.push_str("data: {not json}\n\n");
    engine.script(200, "text/event-stream", stream.as_bytes());
    let response = engine.request("/v1/chat/completions", request).await;
    assert_eq!(response.status, 200);
    let body = String::from_utf8(response.body).expect("public SSE");
    assert!(body.contains("token-19"));
    assert!(!body.ends_with("data: [DONE]\n\n"));
    assert!(body.contains("The inference engine returned an invalid response."));
    assert_eq!(
        engine.chat_attempts(),
        attempts + 2,
        "no retry after commit"
    );

    let failures = engine.route_events("inference_stream_failed");
    assert_eq!(
        failures
            .iter()
            .map(|fields| (
                fields.get("failure_stage").map(String::as_str),
                fields.get("response_committed").map(String::as_str),
                fields.get("attempt_index").map(String::as_str),
                fields.get("route").map(String::as_str),
                fields.get("deployment_id").is_some_and(|id| !id.is_empty()),
            ))
            .collect::<Vec<_>>(),
        [
            (
                Some("sse_decoding"),
                Some("false"),
                Some("0"),
                Some("failure-route"),
                true
            ),
            (
                Some("sse_decoding"),
                Some("true"),
                Some("0"),
                Some("failure-route"),
                true
            ),
        ],
        "each failure is recorded once with its stage and commitment"
    );
    assert!(
        engine.route_event_count() < 20,
        "no event is emitted per streamed token"
    );

    // A complete Responses result with a second choice is refused and attributed once.
    engine.script(
        200,
        "application/json",
        br#"{"id":"r","object":"chat.completion","created":1,"model":"engine-private-model","choices":[{"index":0,"message":{"role":"assistant","content":"one"},"finish_reason":"stop"},{"index":1,"message":{"role":"assistant","content":"two"},"finish_reason":"stop"}]}"#,
    );
    let response = engine
        .request(
            "/v1/responses",
            br#"{"model":"failure-route","store":false,"input":"prompt-sentinel"}"#,
        )
        .await;
    assert_eq!(response.status, 502);
    assert_eq!(
        engine
            .route_events("inference_completion_failed")
            .iter()
            .map(|fields| (
                fields.get("failure_stage").map(String::as_str),
                fields.get("attempt_index").map(String::as_str),
                fields.get("route").map(String::as_str),
                fields.get("deployment_id").is_some_and(|id| !id.is_empty()),
            ))
            .collect::<Vec<_>>(),
        [(
            Some("unexpected_choice"),
            Some("0"),
            Some("failure-route"),
            true
        )]
    );
    engine.assert_backend_logs_private(&["prompt-sentinel", "token-7"]);

    engine.stop().await;
}

async fn fixture_router() -> FixtureRouter {
    fixture_router_with(false, false, Some("fixture-route")).await
}

async fn image_fixture_router() -> FixtureRouter {
    fixture_router_with(true, false, Some("fixture-route")).await
}

async fn direct_route_fixture_router(route_name: &str) -> FixtureRouter {
    fixture_router_with(false, false, Some(route_name)).await
}

async fn direct_route_fixture_router_with_image(route_name: &str) -> FixtureRouter {
    fixture_router_with(true, false, Some(route_name)).await
}

/// A router whose only route is served by a simulated deployment answering decisions only.
async fn decision_fixture_router(route_name: &str) -> FixtureRouter {
    fixture_router_with(false, true, Some(route_name)).await
}

async fn fixture_router_with(
    image_capable: bool,
    decision_only: bool,
    direct_route_name: Option<&str>,
) -> FixtureRouter {
    let root = support::temporary_fast_fixture();
    if decision_only {
        let path = root.join("catalog/models/fixture-model.yaml");
        let model = std::fs::read_to_string(&path).expect("decision fixture model");
        let decision = model.replace(
            "requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']",
            "requestKinds: ['INFERENCE_REQUEST_KIND_DECISION']",
        );
        assert_ne!(decision, model, "fixture model request kinds");
        std::fs::write(path, decision).expect("write decision fixture model");
    }
    if image_capable {
        let path = root.join("catalog/models/fixture-model.yaml");
        let model = std::fs::read_to_string(&path).expect("image fixture model");
        let model = model
            .replace(
                "configurations:",
                "auxiliaryArtifacts:\n  - auxiliaryArtifactId: \"fixture-projector\"\n    source:\n      repository: \"dotcore/fixture\"\n      revision: \"0123456789abcdef0123456789abcdef01234567\"\n      file:\n        path: \"projector.gguf\"\n        provenanceDigest: \"0000000000000000000000000000000000000000000000000000000000000000\"\nconfigurations:",
            )
            .replace(
                "    artifactBindings: [{ bindingId: \"fixture-artifact\", modelArtifactId: \"fixture-artifact\" }]",
                "    artifactBindings: [{ bindingId: \"fixture-artifact\", modelArtifactId: \"fixture-artifact\", auxiliaryArtifactIds: [\"fixture-projector\"] }]",
            )
            .replace(
                "      llamaCpp: { fixedSettings: { gpuLayers: 0 } }",
                "      llamaCpp: { fixedSettings: { gpuLayers: 0, visionProjector: { auxiliaryArtifactId: \"fixture-projector\" } } }",
            )
            .replace(
                "    inputCapabilities: { supportedModalities: [\"MODEL_INPUT_MODALITY_TEXT\"] }",
                "    inputCapabilities:\n      supportedModalities: [\"MODEL_INPUT_MODALITY_TEXT\", \"MODEL_INPUT_MODALITY_IMAGE\"]\n      imageInputCapacity:\n        maximumImageCount: 1\n        supportedMediaTypes: [\"MODEL_IMAGE_MEDIA_TYPE_PNG\"]\n        maximumEncodedBytes: \"1024\"\n        maximumDecodedBytes: \"4096\"\n        maximumWidth: 8\n        maximumHeight: 8\n        maximumPixels: \"64\"\n        fixedVisualTokens: 1\n        fixedDetail: \"MODEL_IMAGE_DETAIL_AUTO\"",
            );
        std::fs::write(path, model).expect("write image fixture model");
    }
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
            exposure: Some(match direct_route_name {
                Some(route_name) => ModelDeploymentExposure {
                    kind: Some(model_deployment_exposure::Kind::Direct(
                        DirectRouteExposure {
                            route_name: route_name.to_owned(),
                        },
                    )),
                },
                None => support::hidden_model_exposure(),
            }),
            model_deployment_name: None,
        }),
    )
    .await
    .expect("load fixture model");
    support::wait_for_model_ready(&model_service).await;
    drop(model_service);
    wait_for_fixture_controller_release(&configuration).await;
    std::fs::remove_file(root.join("catalog/models/fixture-model.yaml"))
        .expect("remove catalog model after immutable deployment snapshot");

    let (shutdown, shutdown_signal) = oneshot::channel();
    let backend = support::prepare_backend(configuration, async move {
        let _ = shutdown_signal.await;
    })
    .await
    .expect("prepared backend");
    let address = backend.addresses().inference.expect("inference listener");
    let management_address = backend.addresses().management.expect("management listener");
    let server = tokio::spawn(backend.serve());
    let management = connect_management(management_address, &server).await;

    let mut router = FixtureRouter {
        _root: root,
        address,
        management,
        shutdown,
        server,
    };
    router.wait_for_model().await;
    if let Some(route_name) = direct_route_name {
        router.wait_for_route(route_name).await;
    }
    router
}

impl FixtureRouter {
    async fn wait_for_route(&mut self, route_name: &str) {
        let mut last_models = None;
        tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
            loop {
                if self.server.is_finished() {
                    let error = (&mut self.server)
                        .await
                        .expect("fixture router task")
                        .expect_err("fixture router failure");
                    panic!("fixture router server: {error}");
                }
                let response = self.request("GET", "/v1/models", b"").await;
                if let Ok(response) = &response
                    && response.status == 200
                    && json_body(response)["data"]
                        .as_array()
                        .is_some_and(|models| models.iter().any(|model| model["id"] == route_name))
                {
                    return;
                }
                last_models = Some(response.map(|response| {
                    (
                        response.status,
                        String::from_utf8_lossy(&response.body).into_owned(),
                    )
                }));
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!("fixture route readiness; last models response: {last_models:?}")
        });
    }

    async fn wait_for_model(&mut self) {
        let mut models = self.model_client();
        let mut deployments = Vec::new();
        tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
            loop {
                if self.server.is_finished() {
                    let error = (&mut self.server)
                        .await
                        .expect("fixture router task")
                        .expect_err("fixture router failure");
                    panic!("fixture router server: {error}");
                }
                deployments = models
                    .list_model_deployments(
                        dotcore_proto::dotcore::model::v1::ListModelDeploymentsRequest {},
                    )
                    .await
                    .expect("list fixture deployments")
                    .into_inner()
                    .deployments;
                if deployments.iter().any(|deployment| {
                    deployment.state.as_ref().is_some_and(|state| {
                        state.status
                            == dotcore_proto::dotcore::model::v1::ModelDeploymentStatus::Ready
                                as i32
                    })
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("fixture router readiness; last deployments: {deployments:?}"));
    }

    fn model_client(
        &self,
    ) -> dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient<
        tonic::transport::Channel,
    >{
        dotcore_proto::dotcore::model::v1::model_deployment_service_client::ModelDeploymentServiceClient::new(
            self.management.clone(),
        )
    }

    fn route_client(
        &self,
    ) -> dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient<
        tonic::transport::Channel,
    > {
        dotcore_proto::dotcore::inference::v1::route_service_client::RouteServiceClient::new(
            self.management.clone(),
        )
    }

    async fn request(&self, method: &str, path: &str, body: &[u8]) -> io::Result<HttpResponse> {
        let mut stream = self.start_request(method, path, body).await?;
        let mut bytes = Vec::new();
        tokio::time::timeout(
            support::CONVERGENCE_DEADLINE,
            stream.read_to_end(&mut bytes),
        )
        .await
        .map_err(|_| {
            io::Error::new(io::ErrorKind::TimedOut, "fixture router response timeout")
        })??;
        parse_response(&bytes)
    }

    async fn start_request(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> io::Result<tokio::net::TcpStream> {
        let mut stream = tokio::net::TcpStream::connect(self.address).await?;
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
            self.address,
            body.len()
        );
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(body).await?;
        Ok(stream)
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("server shutdown");
        tokio::time::timeout(support::CONVERGENCE_DEADLINE, self.server)
            .await
            .expect("server join deadline")
            .expect("server task")
            .expect("server result");
    }
}

async fn read_through_first_sse_frame(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut response = Vec::new();
    let mut byte = [0_u8; 1];
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            stream.read_exact(&mut byte).await.expect("SSE headers");
            response.extend_from_slice(&byte);
            if response.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        loop {
            stream.read_exact(&mut byte).await.unwrap_or_else(|error| {
                panic!(
                    "SSE frame after headers {:?}: {error}",
                    String::from_utf8_lossy(&response)
                )
            });
            response.extend_from_slice(&byte);
            if response.ends_with(b"\n\n") {
                return;
            }
        }
    })
    .await
    .expect("first SSE frame timeout");
    response
}

async fn connect_management(
    address: SocketAddr,
    server: &JoinHandle<dotcore::composition::CompositionResult<()>>,
) -> tonic::transport::Channel {
    let endpoint = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .expect("Management endpoint");
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            if server.is_finished() {
                panic!("fixture router stopped before management connection");
            }
            match endpoint.connect().await {
                Ok(channel) => return channel,
                Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
            }
        }
    })
    .await
    .expect("Management connection deadline")
}

async fn wait_for_fixture_controller_release(configuration: &dotcore_domain::SitePaths) {
    tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
        loop {
            match model_service_for(configuration).await {
                Ok(service) => {
                    drop(service);
                    return;
                }

                Err(error) => {
                    let error = anyhow::Error::from(error);
                    if format!("{error:#}").contains("current state is already locked") {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    } else {
                        panic!("fixture controller release: {error:#}");
                    }
                }
            }
        }
    })
    .await
    .expect("fixture controller release");
}

fn parse_response(bytes: &[u8]) -> io::Result<HttpResponse> {
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP headers are missing"))?;
    let header = std::str::from_utf8(&bytes[..header_end]).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "HTTP headers are invalid UTF-8")
    })?;
    let status = header
        .split_once('\n')
        .map(|(line, _)| line)
        .unwrap_or(header)
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "HTTP status is missing"))?
        .parse()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "HTTP status is invalid"))?;
    Ok(HttpResponse {
        status,
        body: bytes[header_end + 4..].to_vec(),
    })
}

fn json_body(response: &HttpResponse) -> serde_json::Value {
    serde_json::from_slice(&response.body).expect("JSON response body")
}

/// Asserts an image rejection is a 400 naming the rejected cause and the image error code.
fn assert_image_error_names(response: &HttpResponse, expected: &str) {
    let body = json_body(response);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_image");
    let message = body["error"]["message"]
        .as_str()
        .expect("image error message");
    assert!(
        message.contains(expected),
        "{message:?} does not name {expected:?}"
    );
}

fn assert_not_found_route(response: &HttpResponse) {
    assert_eq!(response.status, 404);
    assert_eq!(
        json_body(response),
        serde_json::json!({"error":{"message":"The requested model route does not exist.","type":"invalid_request_error","param":"model","code":"model_not_found"}})
    );
}

fn sse_events(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter_map(|frame| frame.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str(data).expect("SSE JSON event"))
        .collect()
}

fn responses_sse_events(body: &str) -> Vec<(String, serde_json::Value)> {
    body.split("\n\n")
        .filter_map(|frame| frame.split_once("\ndata: "))
        .map(|(event, data)| {
            (
                event
                    .strip_prefix("event: ")
                    .expect("Responses SSE event name")
                    .to_owned(),
                serde_json::from_str(data).expect("Responses SSE JSON"),
            )
        })
        .collect()
}

fn assert_public_fields_absent(body: &[u8]) {
    let value = if let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) {
        value
    } else {
        serde_json::Value::String(String::from_utf8(body.to_vec()).expect("public body is UTF-8"))
    };
    let mut fields = BTreeSet::new();
    collect_object_fields(&value, &mut fields);
    for field in PRIVATE_FIELD_NAMES {
        assert!(!fields.contains(*field), "private field leaked: {field}");
    }
    let body = String::from_utf8_lossy(body).to_ascii_lowercase();
    for marker in [
        "runtime",
        "container",
        "engine_address",
        "host_port",
        "127.0.0.1",
    ] {
        assert!(!body.contains(marker), "private marker leaked: {marker}");
    }
}

fn collect_object_fields(value: &serde_json::Value, fields: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(object) => {
            fields.extend(object.keys().cloned());
            for value in object.values() {
                collect_object_fields(value, fields);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_object_fields(value, fields);
            }
        }
        serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)
        | serde_json::Value::String(_) => {}
    }
}

/// Private model id the fake engine serves; public output must name the route instead.
/// System One request in llama.cpp's dialect asking each question kind of the decision route.
const SYSTEM_ONE_REQUEST: &str = r#"{"model":"decision-route","state":{"ticket":"Refund my order","tier":2},"questions":{"urgent":{"type":"noul","instructions":"Is it urgent?"},"category":{"type":"choice","instructions":"Pick the category.","criteria":{"refund":"Money back","other":null}},"severity":{"type":"score","instructions":"Rate the severity.","criteria":["Minor","Major","Blocking"]}}}"#;

const ENGINE_MODEL: &str = "engine-private-model";
const ENGINE_CREDENTIAL: &str = "engine-credential-sentinel";

/// Pi-shaped streamed Chat request with the intended ninfer sampling and effort controls.
const PI_CHAT_REQUEST: &str = r##"{"model":"qwen3-8-27b","messages":[{"role":"developer","content":"You are Pi."},{"role":"user","content":"Read the README."},{"role":"assistant","content":null,"reasoning":"The file is needed.","tool_calls":[{"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"path\":\"README.md\"}"}}]},{"role":"tool","tool_call_id":"call_1","content":"# Readme"}],"tools":[{"type":"function","function":{"name":"read","description":"Read a file.","parameters":{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]},"strict":false}}],"stream":true,"stream_options":{"include_usage":true},"store":false,"max_completion_tokens":32000,"reasoning_effort":"none","temperature":1.0,"top_p":0.95,"presence_penalty":0.0,"frequency_penalty":0,"top_k":20,"repetition_penalty":1.0,"min_p":0,"seed":18446744073709551615,"chat_template_kwargs":{"enable_thinking":false,"preserve_thinking":true}}"##;

/// Engine stream with separate reasoning, a fragmented function call, and usage details.
const REASONING_TOOL_STREAM: &str = concat!(
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"Let me \"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"check.\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":null,\"function\":{\"name\":null,\"arguments\":\"{\\\"pa\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"th\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":null}\n\n",
    "data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":7,\"model\":\"engine-private-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":30,\"completion_tokens\":12,\"total_tokens\":42,\"prompt_tokens_details\":{\"cached_tokens\":16},\"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n",
    "data: [DONE]\n\n",
);

/// ninfer-shaped text stream with null usage placeholders and final usage details.
const NINFER_USAGE_STREAM: &str = concat!(
    "data: {\"id\":\"n\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}],\"usage\":null}\n\n",
    "data: {\"id\":\"n\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"engine-private-model\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n",
    "data: {\"id\":\"n\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"engine-private-model\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":2,\"total_tokens\":14,\"prompt_tokens_details\":{\"cached_tokens\":4},\"completion_tokens_details\":{\"reasoning_tokens\":1}}}\n\n",
    "data: [DONE]\n\n",
);

const SIMPLE_COMPLETION: &str = r#"{"id":"s","object":"chat.completion","created":1,"model":"engine-private-model","choices":[{"index":0,"message":{"role":"assistant","content":"A pixel."},"finish_reason":"stop"}]}"#;

/// A backend whose only route is served by a configured upstream endpoint on a fake
/// OpenAI-compatible engine, with backend trace events captured for diagnostics checks.
struct EngineRouter {
    _fixture: support::TemporaryFixture,
    backend: support::RunningBackend,
    engine: FakeEngine,
    route: &'static str,
}

impl EngineRouter {
    async fn start(route: &'static str) -> Self {
        trace_capture();
        let engine = FakeEngine::start().await;
        let fixture = support::temporary_fixture();
        let site_path = fixture.join("site.yaml");
        let mut site: dotcore_proto::dotcore::site::v1::Site =
            dotcore_proto::yaml::from_file(&site_path).expect("Site fixture");
        site.execution_targets.clear();
        site.model_deployments.clear();
        std::fs::write(
            &site_path,
            dotcore_proto::yaml::to_string(&site).expect("site YAML"),
        )
        .expect("upstream site fixture");
        std::fs::set_permissions(&site_path, std::fs::Permissions::from_mode(0o600))
            .expect("Site permissions");
        let backend = support::RunningBackend::start(support::configuration(&fixture)).await;
        backend
            .model_client()
            .create_model_deployment(CreateModelDeploymentRequest {
                source: Some(
                    dotcore_proto::dotcore::model::v1::create_model_deployment_request::Source::Upstream(
                        CreateUpstreamModelDeploymentRequest {
                            upstream_model_template_id: "openai-compatible".to_owned(),
                            desired_configuration: Some(UpstreamModelDesiredConfiguration {
                                upstream_model_id: ENGINE_MODEL.to_owned(),
                                api_key: ENGINE_CREDENTIAL.to_owned(),
                                input_capabilities: Some(UpstreamModelInputCapabilities {
                                    supports_text: true,
                                    supports_image: true,
                                }),
                                endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                                    base_url: format!("http://{}/v1", engine.address),
                                    allow_insecure_http: true,
                                }),
                                boundary: Some(UpstreamModelBoundary {
                                    locality: UpstreamModelLocality::Private as i32,
                                    trust_domain: "engine.test".to_owned(),
                                    data_egress_class: UpstreamModelDataEgressClass::PrivateNetwork
                                        as i32,
                                }),
                            }),
                        },
                    ),
                ),
                exposure: Some(ModelDeploymentExposure {
                    kind: Some(model_deployment_exposure::Kind::Direct(DirectRouteExposure {
                        route_name: route.to_owned(),
                    })),
                }),
                model_deployment_name: None,
            })
            .await
            .expect("create upstream deployment");
        let router = Self {
            _fixture: fixture,
            backend,
            engine,
            route,
        };
        tokio::time::timeout(support::CONVERGENCE_DEADLINE, async {
            loop {
                let models = router.request_raw("GET", "/v1/models", b"").await;
                if models.status == 200
                    && json_body(&models)["data"]
                        .as_array()
                        .is_some_and(|models| models.iter().any(|model| model["id"] == route))
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("upstream route readiness");
        router
    }

    /// Queues the fake engine's answer to the next Chat request.
    fn script(&self, status: u16, content_type: &'static str, body: &[u8]) {
        self.engine
            .responses
            .lock()
            .expect("engine responses")
            .push_back((status, content_type, body.to_vec()));
    }

    fn chat_attempts(&self) -> usize {
        self.engine.requests.lock().expect("engine requests").len()
    }

    /// Raw body of the latest Chat request the engine received.
    fn last_request(&self) -> String {
        String::from_utf8(
            self.engine
                .requests
                .lock()
                .expect("engine requests")
                .last()
                .expect("an engine request")
                .clone(),
        )
        .expect("UTF-8 engine request")
    }

    async fn request(&self, path: &str, body: &[u8]) -> HttpResponse {
        let response = self.request_raw("POST", path, body).await;
        assert_public_fields_absent(&response.body);
        let public = String::from_utf8_lossy(&response.body);
        for private in [ENGINE_MODEL, ENGINE_CREDENTIAL, "engine-body-sentinel"] {
            assert!(
                !public.contains(private),
                "{private} leaked into a public response"
            );
        }
        response
    }

    async fn request_raw(&self, method: &str, path: &str, body: &[u8]) -> HttpResponse {
        let mut stream = tokio::net::TcpStream::connect(self.backend.inference_address())
            .await
            .expect("inference connection");
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .await
            .expect("request head");
        stream.write_all(body).await.expect("request body");
        let mut bytes = Vec::new();
        tokio::time::timeout(
            support::CONVERGENCE_DEADLINE,
            stream.read_to_end(&mut bytes),
        )
        .await
        .expect("inference response deadline")
        .expect("inference response");
        parse_response(&bytes).expect("HTTP response")
    }

    /// Backend events of `event_kind` emitted while serving this router's route.
    fn route_events(&self, event_kind: &str) -> Vec<BTreeMap<String, String>> {
        trace_capture()
            .events
            .lock()
            .expect("captured events")
            .iter()
            .filter(|event| event.route.as_deref() == Some(self.route))
            .filter(|event| event.fields.get("event_kind").map(String::as_str) == Some(event_kind))
            .map(|event| event.fields.clone())
            .collect()
    }

    fn route_event_count(&self) -> usize {
        trace_capture()
            .events
            .lock()
            .expect("captured events")
            .iter()
            .filter(|event| event.route.as_deref() == Some(self.route))
            .count()
    }

    /// Asserts that no backend log line carries a payload value, credential, private model
    /// id, or private endpoint.
    fn assert_backend_logs_private(&self, sentinels: &[&str]) {
        let output = trace_capture()
            .output
            .lock()
            .expect("captured output")
            .clone();
        let output = String::from_utf8_lossy(&output);
        let endpoint = self.engine.address.to_string();
        assert!(
            output.lines().any(|line| line.contains(self.route)),
            "the route's requests must have produced backend log lines"
        );
        for line in output.lines().filter(|line| line.contains(self.route)) {
            for private in sentinels.iter().copied().chain([
                ENGINE_MODEL,
                ENGINE_CREDENTIAL,
                endpoint.as_str(),
            ]) {
                assert!(
                    !line.contains(private),
                    "{private} leaked into a log line: {line}"
                );
            }
        }
        for line in output.lines() {
            for private in sentinels.iter().copied().chain([ENGINE_CREDENTIAL]) {
                assert!(
                    !line.contains(private),
                    "{private} leaked into a log line: {line}"
                );
            }
        }
    }

    async fn stop(self) {
        self.backend.stop().await;
        self.engine.task.abort();
    }
}

/// Fake OpenAI-compatible engine: answers model probes and plays queued Chat answers in order,
/// recording each Chat request body.
struct FakeEngine {
    address: SocketAddr,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    responses: ScriptedAnswers,
    task: JoinHandle<()>,
}

impl FakeEngine {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("engine listener");
        let address = listener.local_addr().expect("engine address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let responses = Arc::new(Mutex::new(VecDeque::new()));
        let task = tokio::spawn({
            let requests = Arc::clone(&requests);
            let responses = Arc::clone(&responses);
            async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve_engine_request(
                        stream,
                        Arc::clone(&requests),
                        Arc::clone(&responses),
                    ));
                }
            }
        });
        Self {
            address,
            requests,
            responses,
            task,
        }
    }
}

/// Queued engine answers as status, content type, and body.
type ScriptedAnswers = Arc<Mutex<VecDeque<(u16, &'static str, Vec<u8>)>>>;

async fn serve_engine_request(
    mut stream: tokio::net::TcpStream,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    responses: ScriptedAnswers,
) {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 8192];
    let (header_end, length) = loop {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
                .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while bytes.len() < header_end + length {
        let Ok(read) = stream.read(&mut buffer).await else {
            return;
        };
        if read == 0 {
            return;
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    let (status, content_type, body) = if bytes.starts_with(b"GET ") {
        (
            200,
            "application/json",
            format!(r#"{{"id":"{ENGINE_MODEL}","object":"model"}}"#).into_bytes(),
        )
    } else {
        requests
            .lock()
            .expect("engine requests")
            .push(bytes[header_end..header_end + length].to_vec());
        responses
            .lock()
            .expect("engine responses")
            .pop_front()
            .unwrap_or((
                500,
                "application/json",
                br#"{"error":"unscripted"}"#.to_vec(),
            ))
    };
    let head = format!(
        "HTTP/1.1 {status} Engine\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(&body).await;
}

/// Backend trace events and formatted log lines captured for the whole test binary.
/// Fields of the requests refused before routing, in the order they were logged, without the
/// event's own text and kind.
fn rejected_request_events() -> Vec<BTreeMap<String, String>> {
    trace_capture()
        .events
        .lock()
        .expect("captured events")
        .iter()
        .filter(|event| {
            event.fields.get("event_kind").map(String::as_str) == Some("inference_request_rejected")
        })
        .map(|event| {
            let mut fields = event.fields.clone();
            fields.remove("event_kind");
            fields.remove("message");
            fields
        })
        .collect()
}

#[derive(Clone, Default)]
struct TraceCapture {
    events: Arc<Mutex<Vec<CapturedEvent>>>,
    output: Arc<Mutex<Vec<u8>>>,
}

/// One backend event with its fields and the route of the request that emitted it.
struct CapturedEvent {
    route: Option<String>,
    fields: BTreeMap<String, String>,
}

fn trace_capture() -> &'static TraceCapture {
    static CAPTURE: OnceLock<TraceCapture> = OnceLock::new();
    CAPTURE.get_or_init(|| {
        let capture = TraceCapture::default();
        let output = Arc::clone(&capture.output);
        tracing_subscriber::registry()
            .with(CaptureLayer(capture.clone()))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(move || CaptureWriter(Arc::clone(&output)))
                    .with_filter(tracing_subscriber::EnvFilter::new("dotcore=trace")),
            )
            .try_init()
            .expect("the trace capture is the only subscriber");
        capture
    })
}

struct CaptureLayer(TraceCapture);

impl<S> tracing_subscriber::Layer<S> for CaptureLayer
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut fields = BTreeMap::new();
        attributes.record(&mut FieldVisitor(&mut fields));
        if let Some(span) = context.span(id) {
            span.extensions_mut().insert(fields);
        }
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if let Some(span) = context.span(id)
            && let Some(fields) = span.extensions_mut().get_mut::<BTreeMap<String, String>>()
        {
            values.record(&mut FieldVisitor(fields));
        }
    }

    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        if !event.metadata().target().starts_with("dotcore") {
            return;
        }
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        let route = context.event_scope(event).and_then(|scope| {
            scope.into_iter().find_map(|span| {
                span.extensions()
                    .get::<BTreeMap<String, String>>()
                    .and_then(|fields| fields.get("model_id").cloned())
            })
        });
        self.0
            .events
            .lock()
            .expect("captured events")
            .push(CapturedEvent { route, fields });
    }
}

struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("captured output")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
