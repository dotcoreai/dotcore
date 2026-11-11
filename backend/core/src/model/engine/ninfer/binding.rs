//! Binds one ninfer loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt and preserves cancellation,
//! fallback disposition, streaming, and passthrough response evidence for the router.
//! It recognizes ninfer's refusal of a chat prompt too large for the deployment, so clients learn
//! to shorten the request instead of seeing an engine failure.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory, InvalidRequest};
use crate::model::engine::loopback_http::{LoopbackInvoker, RejectionClassifiers};

/// Creates endpoint-bound ninfer transport invokers for engine bindings.
pub(crate) struct NinferTransportFactory;

impl EngineTransportFactory for NinferTransportFactory {
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

/// Recognizes ninfer's refusal of a chat prompt larger than the deployment's context.
///
/// ninfer names that refusal with its own error code on an HTTP 400, so the code is matched
/// rather than the message.
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
    use super::*;

    #[test]
    fn only_a_context_length_refusal_is_an_invalid_request() {
        let refusal = |code: &str| {
            serde_json::to_vec(&serde_json::json!({"error":{"message":"prepared prompt exceeds Engine max_context 4096","type":"invalid_request_error","param":null,"code":code}}))
                .expect("error body")
        };

        assert_eq!(
            classify_chat_rejection(400, &refusal("context_length_exceeded")),
            Some(InvalidRequest::context_length())
        );
        for (status, body) in [
            (500, refusal("context_length_exceeded")),
            (400, refusal("media_budget_exceeded")),
            (400, b"context_length_exceeded".to_vec()),
        ] {
            assert_eq!(classify_chat_rejection(status, &body), None, "{status}");
        }
    }
}
