//! Binds one vLLM loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt and preserves cancellation,
//! fallback disposition, streaming, and passthrough response evidence for the router.
//! It recognizes vLLM's refusals of a chat prompt too large for the deployment, so clients learn
//! to shorten the request instead of seeing an engine failure.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory, InvalidRequest};
use crate::model::engine::loopback_http::{
    LoopbackInvoker, RejectionClassifiers, error_message, matches_format,
};

/// Creates endpoint-bound vLLM transport invokers for engine bindings.
pub(crate) struct VllmTransportFactory;

impl EngineTransportFactory for VllmTransportFactory {
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

/// Recognizes vLLM's refusal of a chat prompt larger than the deployment's context.
///
/// Only an exact status and message match is recognized, so a reworded engine message keeps
/// passing through rather than being misread.
fn classify_chat_rejection(status: u16, body: &[u8]) -> Option<InvalidRequest> {
    let message = error_message(body)?;
    (status == 400
        && OVERSIZED_PROMPT_REJECTIONS
            .iter()
            .any(|format| matches_format(&message, format)))
    .then(InvalidRequest::context_length)
}

/// vLLM's refusals of a prompt larger than the deployment's context, which it answers with HTTP
/// 400. It counts the prompt's tokens, or first bounds its characters, and may report a token
/// count that is only a lower bound (v0.32.0 `vllm/renderers/params.py`).
const OVERSIZED_PROMPT_REJECTIONS: [&str; 3] = [
    "This model's maximum context length is %d tokens. However, your prompt contains %d input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt. (parameter=input_tokens, value=%d)",
    "This model's maximum context length is %d tokens. However, your prompt contains at least %d input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt. (parameter=input_tokens, value=%d)",
    "This model's maximum context length is %d tokens. However, your prompt contains %d characters (more than %d characters, which is the upper bound for %d input tokens). Please reduce the length of the input prompt. (parameter=input_text, value=%d)",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_prompt_refusals_are_context_length_invalid_requests() {
        for message in [
            "This model's maximum context length is 4096 tokens. However, your prompt contains 5000 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt. (parameter=input_tokens, value=5000)",
            "This model's maximum context length is 4096 tokens. However, your prompt contains at least 4097 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt. (parameter=input_tokens, value=4097)",
            "This model's maximum context length is 4096 tokens. However, your prompt contains 90000 characters (more than 81920 characters, which is the upper bound for 4096 input tokens). Please reduce the length of the input prompt. (parameter=input_text, value=90000)",
        ] {
            assert_eq!(
                classify_chat_rejection(400, &body(message)),
                Some(InvalidRequest::context_length()),
                "{message}"
            );
        }
    }

    #[test]
    fn other_refusals_pass_through() {
        let oversized = "This model's maximum context length is 4096 tokens. However, your prompt contains 5000 input tokens, which exceeds this limit even without any output tokens. Please reduce the length of the input prompt. (parameter=input_tokens, value=5000)";
        for (status, body) in [
            (500, body(oversized)),
            (
                400,
                body("max_tokens must be at least 1, got 0. (parameter=max_tokens, value=0)"),
            ),
            (400, oversized.as_bytes().to_vec()),
        ] {
            assert_eq!(classify_chat_rejection(status, &body), None, "{status}");
        }
    }

    /// vLLM's error body for `message`.
    fn body(message: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"error":{"message":message,"type":"BadRequestError","param":"input_tokens","code":400}}))
            .expect("error body")
    }
}
