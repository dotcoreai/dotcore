//! Binds one oMLX loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt through the shared OpenAI-compatible
//! loopback transport and preserves cancellation, fallback disposition, streaming, and
//! passthrough response evidence for the router. It recognizes oMLX's refusal of a chat prompt
//! too large for the deployment, so clients learn to shorten the request instead of seeing an
//! engine failure. Every other refusal, such as its memory guard turning a prompt away, passes
//! through unclassified.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory, InvalidRequest};
use crate::model::engine::loopback_http::{LoopbackInvoker, RejectionClassifiers};

/// Creates endpoint-bound oMLX transport invokers for engine bindings.
pub(crate) struct OmlxTransportFactory;

impl EngineTransportFactory for OmlxTransportFactory {
    fn bind_loopback(&self, port: u16, served_model_id: String) -> Arc<dyn EngineInvoker> {
        LoopbackInvoker::bind(
            port,
            served_model_id,
            super::REQUEST_KINDS,
            RejectionClassifiers {
                chat: Some(classify_chat_rejection),
                decision: None,
            },
        )
    }
}

/// Recognizes oMLX's refusal of a chat prompt larger than the deployment's context.
///
/// oMLX names that refusal with OpenAI's error code on an HTTP 400, so the code is matched rather
/// than the message.
fn classify_chat_rejection(status: u16, body: &[u8]) -> Option<InvalidRequest> {
    let body = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    (status == 400
        && body
            .pointer("/error/code")
            .and_then(serde_json::Value::as_str)
            == Some("context_length_exceeded"))
    .then(InvalidRequest::context_length)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use serde_json::{Value, json};

    use super::*;
    use crate::{
        inference::{InferenceAttempt, InferenceError, InferenceResponse, inference_cancellation},
        router::openai::{self, OpenAiRequest},
    };

    /// oMLX 0.8.4's answer to a prompt above the context per request (`server.py`,
    /// `_ContextLengthExceededError`).
    const OVERSIZED_PROMPT: &str = r#"{"error":{"message":"Prompt is too long: 40010 tokens > 32768 maximum context length","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;

    /// oMLX 0.8.4's memory guard turning a prompt away before generation: an HTTP 400 with no code.
    const MEMORY_GUARD_REFUSAL: &str = r#"{"error":{"message":"oMLX prefill memory guard rejected this prompt: Prefill would require ~9.1 GB peak but the prefill limit is 8.6 GB","type":"invalid_request_error","param":null,"code":null}}"#;

    #[test]
    fn only_a_context_length_refusal_is_an_invalid_request() {
        assert_eq!(
            classify_chat_rejection(400, OVERSIZED_PROMPT.as_bytes()),
            Some(InvalidRequest::context_length())
        );
        for (status, body) in [
            (500, OVERSIZED_PROMPT.as_bytes()),
            (400, MEMORY_GUARD_REFUSAL.as_bytes()),
            (400, b"context_length_exceeded".as_slice()),
        ] {
            assert_eq!(classify_chat_rejection(status, body), None, "{status}");
        }
    }

    #[tokio::test]
    async fn an_oversized_prompt_fails_as_a_context_length_invalid_request() {
        let attempt = invoke_answering(400, OVERSIZED_PROMPT).await;

        let InferenceAttempt::Failed { error, .. } = attempt else {
            panic!("classified refusal");
        };
        assert!(
            matches!(&error, InferenceError::InvalidRequest(invalid) if *invalid == InvalidRequest::context_length()),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_memory_guard_refusal_passes_through_unclassified() {
        let attempt = invoke_answering(400, MEMORY_GUARD_REFUSAL).await;

        let InferenceAttempt::Response(InferenceResponse::Passthrough(response)) = attempt else {
            panic!("passthrough response");
        };
        assert_eq!(response.status, 400);
        assert_eq!(response.body, MEMORY_GUARD_REFUSAL.as_bytes());

        // The router projects the passed-through refusal into its safe envelope, keeping the
        // client error status.
        let public = openai::upstream(response.status, Vec::new(), response.body);
        assert_eq!(public.status, 400);
        assert_eq!(
            serde_json::from_slice::<Value>(&public.body).expect("error JSON"),
            json!({"error":{"message":"The inference engine rejected the request.","type":"upstream_error","param":null,"code":null}})
        );
    }

    /// Sends one chat request through an oMLX binding to a loopback engine answering `status`
    /// with the JSON `body`.
    async fn invoke_answering(status: u16, body: &str) -> InferenceAttempt {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("engine listener");
        let port = listener.local_addr().expect("engine address").port();
        let response = format!(
            "HTTP/1.1 {status} Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let engine = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("engine connection");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = std::io::Read::read(&mut stream, &mut buffer).expect("request bytes");
                request.extend_from_slice(&buffer[..read]);
                if read == 0 || request_complete(&request) {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).expect("response");
        });
        let OpenAiRequest::Chat(request) = openai::decode(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"route","messages":[{"role":"user","content":"hi"}]}"#,
        )
        .expect("chat request") else {
            panic!("chat request");
        };

        let (_signal, cancellation) = inference_cancellation();
        let attempt = OmlxTransportFactory
            .bind_loopback(port, "served-model".to_owned())
            .invoke(request, cancellation)
            .await;
        engine.join().expect("engine thread");
        attempt
    }

    /// Whether a buffered HTTP request holds its headers and the body they declare.
    fn request_complete(request: &[u8]) -> bool {
        let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            return false;
        };
        let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
        let length = headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or_default();
        request.len() >= end + 4 + length
    }
}
