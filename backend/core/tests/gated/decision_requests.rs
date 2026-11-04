//! Shared System One requests and response assertions of the gated real-engine decision tests.
//!
//! Both hardware suites deploy clef-flash with a total context small enough to overflow, send the
//! same research-example request and an oversized one through the router's `/v1/systemone` route,
//! and check the response structure and error shapes. Model probabilities are never asserted.

use dotcore_proto::dotcore::{
    catalog::v1::engine_parameter_value, model::v1::LocalModelDeploymentConfiguration,
};
use serde_json::{Value, json};

use crate::support;

/// The production-sized decision model the suites deploy, with its smallest quantization.
pub const MODEL_ID: &str = "clef-flash";
pub const PROFILE_ID: &str = "q4-k-m";

/// Total context the suites deploy clef-flash with; the oversized request exceeds it.
pub const DECISION_TOTAL_CONTEXT: u64 = 4096;

const MODEL_ARTIFACT_BINDING_ID: &str = "q4-k-m";

/// Question names of the research example, in request order.
const QUESTION_NAMES: [&str; 3] = ["topic", "urgency", "refund"];

/// Bytes of state in the oversized request, far above any tokenization of the total context.
const OVERSIZED_STATE_BYTES: usize = 200_000;

/// Deployment configuration of the Q4_K_M artifact with `DECISION_TOTAL_CONTEXT` and one request
/// at a time, so the whole total context serves a single prompt.
pub fn decision_model_configuration() -> LocalModelDeploymentConfiguration {
    let mut configuration =
        support::local_model_configuration(MODEL_ARTIFACT_BINDING_ID, DECISION_TOTAL_CONTEXT);
    configuration
        .engine_parameter_values
        .as_mut()
        .expect("engine parameter values")
        .values
        .iter_mut()
        .find(|value| value.parameter_id == "parallel-requests")
        .expect("parallel requests parameter value")
        .value = Some(engine_parameter_value::Value::UnsignedInteger(1));
    configuration
}

/// The research example as a raw HTTP request: a choice, a score, and a predicate question.
pub fn research_example_http(route: &str) -> Vec<u8> {
    // Written as text because `Value` objects sort their keys, and the question order is the
    // behavior under test.
    let body = format!(
        r#"{{"model":{route:?},"state":"Customer writes: My invoice was charged twice this month and I want my money back today.","questions":{{
            "topic":{{"type":"choice","instructions":"What is the main topic of the message?","criteria":{{"billing":"Charges, invoices, refunds","technical":"Product malfunction","other":null}}}},
            "urgency":{{"type":"score","instructions":"How urgent is the request?","criteria":["not urgent","somewhat urgent","urgent"]}},
            "refund":{{"type":"noul","instructions":"Does the customer ask for a refund?"}}}}}}"#
    );
    http_post(body.into_bytes())
}

/// A decision request whose state alone exceeds the total context, as a raw HTTP request.
pub fn oversized_http(route: &str) -> Vec<u8> {
    http_post(
        serde_json::to_vec(&json!({
            "model": route,
            "state": "refund ".repeat(OVERSIZED_STATE_BYTES / "refund ".len()),
            "questions": {
                "refund": {"type": "noul", "instructions": "Does the customer ask for a refund?"},
            },
        }))
        .expect("oversized request payload"),
    )
}

/// Asserts a 200 research-example response: answers in request order with the expected shapes,
/// probabilities summing to one, and no generated tokens.
pub fn assert_research_example_response(route: &str, response: &[u8]) {
    let (status, body) = split_response(response);
    assert_eq!(status, 200, "research example failed: {body}");
    let positions = QUESTION_NAMES.map(|name| {
        body.find(&format!("\"{name}\":{{\"type\""))
            .unwrap_or_else(|| panic!("answer {name:?} is missing from {body}"))
    });
    assert!(
        positions.is_sorted(),
        "answers are not in request order: {body}"
    );

    let value: Value = serde_json::from_str(&body).expect("decision response JSON");
    assert_eq!(value["model"], route);
    let answers = &value["answers"];
    assert_eq!(answers.as_object().map(|answers| answers.len()), Some(3));

    let topic = &answers["topic"];
    assert_eq!(topic["type"], "choice");
    assert!(["billing", "technical", "other"].contains(&topic["choice"].as_str().unwrap_or("")));
    assert_probabilities(&topic["probabilities"], &["billing", "technical", "other"]);

    let urgency = &answers["urgency"];
    assert_eq!(urgency["type"], "score");
    assert_eq!(
        urgency["legend"],
        json!({"0": "not urgent", "1": "somewhat urgent", "2": "urgent"})
    );
    assert_probabilities(&urgency["probabilities"], &["0", "1", "2"]);
    let score = urgency["score"].as_f64().expect("score is a number");
    assert!((0.0..=2.0).contains(&score), "score {score} outside 0..=2");

    let refund = &answers["refund"];
    assert_eq!(refund["type"], "noul");
    let noul = refund["noul"].as_f64().expect("noul is a number");
    assert!((0.0..=1.0).contains(&noul), "noul {noul} outside 0..=1");

    assert!(
        value["usage"]["input_tokens"].as_u64().unwrap_or(0) > 0,
        "usage has no input tokens: {body}"
    );
    assert_eq!(value["usage"]["output_tokens"], 0);
}

/// Asserts the exact client error of a decision request larger than the deployment's context.
pub fn assert_context_length_exceeded_response(response: &[u8]) {
    let (status, body) = split_response(response);
    assert_eq!(status, 400, "oversized request response: {body}");
    let value: Value = serde_json::from_str(&body).expect("error response JSON");
    assert_eq!(
        value,
        json!({"error": {
            "type": "invalid_request_error",
            "code": "context_length_exceeded",
            "message": "The request is larger than the deployment's context.",
            "param": null,
        }})
    );
}

fn assert_probabilities(probabilities: &Value, names: &[&str]) {
    let probabilities = probabilities.as_object().expect("probabilities object");
    let mut keys = probabilities.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    let mut expected = names.to_vec();
    expected.sort_unstable();
    assert_eq!(keys, expected);
    let sum = probabilities
        .values()
        .map(|value| value.as_f64().expect("probability is a number"))
        .sum::<f64>();
    assert!(
        (sum - 1.0).abs() < 1e-3,
        "probabilities sum to {sum}: {probabilities:?}"
    );
}

fn http_post(body: Vec<u8>) -> Vec<u8> {
    let mut request = format!(
        "POST /v1/systemone HTTP/1.1\r\nHost: test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    request.extend_from_slice(&body);
    request
}

fn split_response(response: &[u8]) -> (u16, String) {
    let text = String::from_utf8_lossy(response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .expect("router HTTP response headers");
    let status = head
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("router status line in {head:?}"));
    (status, body.to_owned())
}
