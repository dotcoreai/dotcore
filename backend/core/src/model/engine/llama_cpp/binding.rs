//! Binds one llama.cpp loopback service to the engine-neutral inference contract.
//!
//! The binding performs exactly one transport attempt and preserves cancellation,
//! fallback disposition, streaming, and passthrough response evidence for the router.
//! It recognizes llama.cpp's refusals of a decision prompt too large for the deployment, so
//! clients learn to shorten the request instead of seeing an engine failure.

use std::sync::Arc;

use crate::inference::{EngineInvoker, EngineTransportFactory, InvalidRequest};
use crate::model::engine::loopback_http::LoopbackInvoker;

/// Creates endpoint-bound llama.cpp transport invokers for engine bindings.
pub(crate) struct LlamaCppTransportFactory;

impl EngineTransportFactory for LlamaCppTransportFactory {
    fn bind_loopback(&self, port: u16, served_model_id: String) -> Arc<dyn EngineInvoker> {
        LoopbackInvoker::bind(
            port,
            served_model_id,
            super::REQUEST_KINDS,
            Some(classify_decision_rejection),
        )
    }
}

/// Recognizes llama.cpp's refusal of a decision prompt larger than the deployment's context.
///
/// Only an exact status and message match is recognized, so a reworded engine message keeps
/// passing through rather than being misread.
fn classify_decision_rejection(status: u16, body: &[u8]) -> Option<InvalidRequest> {
    let body = serde_json::from_slice::<serde_json::Value>(body).ok()?;
    let message = body.pointer("/error/message")?.as_str()?;
    OVERSIZED_PROMPT_REJECTIONS
        .iter()
        .any(|(rejected_status, format)| {
            *rejected_status == status && matches_format(message, format)
        })
        .then(InvalidRequest::context_length)
}

/// llama.cpp's refusals of an oversized decision prompt: the HTTP status it answers with and its
/// message format, where `%d` stands for a token count.
///
/// A prompt larger than the micro-batch is a server error, and one larger than a slot's context
/// is a context-size error (b11723 `tools/server/server-context.cpp`).
const OVERSIZED_PROMPT_REJECTIONS: [(u16, &str); 2] = [
    (
        500,
        "input (%d tokens) is too large to process. increase the physical batch size (current batch size: %d)",
    ),
    (
        400,
        "input (%d tokens) is larger than the max context size (%d tokens). skipping",
    ),
];

/// Whether `message` is `format` with each `%d` replaced by a decimal number.
fn matches_format(message: &str, format: &str) -> bool {
    let mut fragments = format.split("%d");
    let Some(mut remaining) = fragments
        .next()
        .and_then(|first| message.strip_prefix(first))
    else {
        return false;
    };
    for fragment in fragments {
        let digits = remaining.bytes().take_while(u8::is_ascii_digit).count();
        let Some(rest) = (digits > 0)
            .then(|| remaining[digits..].strip_prefix(fragment))
            .flatten()
        else {
            return false;
        };
        remaining = rest;
    }
    remaining.is_empty()
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatMessage, ChatMessageRole, ChatRequest, DecisionContentPart, DecisionPredicate,
        DecisionQuestion, DecisionRequest, chat_message, decision_content_part::Content,
        decision_question::Kind,
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    use super::*;
    use crate::inference::{
        DecisionResponse, InferenceAttempt, InferenceAttemptDisposition, InferenceError,
        InferenceResponse, inference_cancellation,
    };

    const BATCH_REJECTION: &str = r#"{"error":{"code":500,"message":"input (5000 tokens) is too large to process. increase the physical batch size (current batch size: 4096)","type":"server_error"}}"#;

    const CONTEXT_REJECTION: &str = r#"{"error":{"code":400,"message":"input (5000 tokens) is larger than the max context size (4096 tokens). skipping","type":"exceed_context_size_error","n_prompt_tokens":5000,"n_ctx":4096}}"#;

    #[tokio::test]
    async fn oversized_decision_rejections_fail_as_context_length_invalid_requests() {
        for (status, body) in [(500, BATCH_REJECTION), (400, CONTEXT_REJECTION)] {
            let attempt = decide(respond(status, body)).await;

            assert!(
                matches!(
                    attempt,
                    InferenceAttempt::Failed {
                        disposition: InferenceAttemptDisposition::NonRetryable,
                        error: InferenceError::InvalidRequest(error),
                    } if error == InvalidRequest::context_length()
                ),
                "a {status} oversized-prompt rejection must be an invalid request"
            );
        }
    }

    #[tokio::test]
    async fn oversized_prompt_messages_with_another_status_pass_through_unchanged() {
        for (status, body) in [(400, BATCH_REJECTION), (500, CONTEXT_REJECTION)] {
            let attempt = decide(respond(status, body)).await;

            assert_passthrough(attempt, status, body);
        }
    }

    #[tokio::test]
    async fn other_decision_rejections_pass_through_unchanged() {
        for (status, body) in [
            (
                500,
                r#"{"error":{"code":500,"message":"the current context does not support logits computation. skipping","type":"server_error"}}"#,
            ),
            (
                500,
                r#"{"error":{"code":500,"message":"input (many tokens) is too large to process. increase the physical batch size (current batch size: 4096)","type":"server_error"}}"#,
            ),
            (
                500,
                r#"{"error":{"code":500,"message":"input (5000 tokens) is too large to process. increase the physical batch size (current batch size: 4096). retry","type":"server_error"}}"#,
            ),
            (500, "input (5000 tokens) is too large to process."),
        ] {
            let attempt = decide(respond(status, body)).await;

            assert_passthrough(attempt, status, body);
        }
    }

    #[tokio::test]
    async fn chat_rejections_are_never_classified() {
        for (status, body) in [(500, BATCH_REJECTION), (400, CONTEXT_REJECTION)] {
            let (port, engine) = serve(respond(status, body)).await;
            let invoker = LlamaCppTransportFactory.bind_loopback(port, "served".to_owned());

            let attempt = invoker
                .invoke(chat_request(), inference_cancellation().1)
                .await;
            engine.await.expect("engine task");

            let InferenceAttempt::Response(InferenceResponse::Passthrough(passthrough)) = attempt
            else {
                panic!("a chat {status} rejection must pass through");
            };
            assert_eq!(
                (passthrough.status, passthrough.body),
                (status, body.as_bytes().to_vec())
            );
        }
    }

    #[test]
    fn formats_match_only_whole_messages_with_numbers_in_place() {
        let format = "input (%d tokens) is larger than the max context size (%d tokens). skipping";

        assert!(matches_format(
            "input (17 tokens) is larger than the max context size (16 tokens). skipping",
            format
        ));
        for message in [
            "input ( tokens) is larger than the max context size (16 tokens). skipping",
            "input (17 tokens) is larger than the max context size (-16 tokens). skipping",
            "input (17 tokens) is larger than the max context size (16 tokens).",
            "error: input (17 tokens) is larger than the max context size (16 tokens). skipping",
        ] {
            assert!(!matches_format(message, format), "{message}");
        }
    }

    /// Decides one request through a llama.cpp binding whose engine answers `response`.
    async fn decide(response: String) -> InferenceAttempt<DecisionResponse> {
        let (port, engine) = serve(response).await;
        let invoker = LlamaCppTransportFactory.bind_loopback(port, "served".to_owned());

        let attempt = invoker
            .invoke_decision(decision_request(), inference_cancellation().1)
            .await;
        engine.await.expect("engine task");
        attempt
    }

    fn assert_passthrough(attempt: InferenceAttempt<DecisionResponse>, status: u16, body: &str) {
        let InferenceAttempt::Response(DecisionResponse::Passthrough(passthrough)) = attempt else {
            panic!("a {status} rejection must pass through: {body}");
        };
        assert_eq!(
            (passthrough.status, passthrough.body),
            (status, body.as_bytes().to_vec())
        );
    }

    /// Serves one canned `response` on a loopback port to the first request.
    async fn serve(response: String) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("engine listener");
        let port = listener.local_addr().expect("engine address").port();
        let engine = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.expect("engine connection");
            crate::test_support::http::read_request(&mut connection).await;
            connection
                .write_all(response.as_bytes())
                .await
                .expect("engine response");
        });
        (port, engine)
    }

    fn respond(status: u16, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn decision_request() -> DecisionRequest {
        DecisionRequest {
            model_id: "decisions".to_owned(),
            state: vec![DecisionContentPart {
                content: Some(Content::TextContent("ticket".to_owned())),
            }],
            questions: vec![DecisionQuestion {
                name: "urgent".to_owned(),
                instructions: vec![DecisionContentPart {
                    content: Some(Content::TextContent("Is it urgent?".to_owned())),
                }],
                kind: Some(Kind::Predicate(DecisionPredicate::default())),
            }],
        }
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            model_id: "chat".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent("hello".to_owned())),
                ..Default::default()
            }],
            stream: Some(false),
            ..Default::default()
        }
    }
}
