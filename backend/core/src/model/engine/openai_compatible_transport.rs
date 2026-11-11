//! Loopback chat transport for local OpenAI-compatible engines.
//!
//! The llama.cpp, vLLM, and ninfer bindings share this one-attempt transport: it sends the
//! validated request under the engine's served model id, with its instruction messages shaped for
//! model chat templates, decodes complete or streamed answers,
//! and passes unsuccessful engine responses through unchanged. Streams are checked for order
//! and completeness before any event reaches the router.

#[cfg(test)]
use dotcore_proto::dotcore::inference::v1::{ChatMessage, ChatMessageRole, chat_message};
use dotcore_proto::dotcore::inference::v1::{
    ChatRequest, ChatStreamCompleted, ChatStreamEvent, chat_stream_event::Event,
};
#[cfg(test)]
use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, net::TcpStream, sync::mpsc};
use tracing::{Instrument, debug, debug_span, field};

use super::instruction_roles;
use super::loopback_http::{
    self as http, ChunkedBodyDecoder, RejectionClassifier, inference_attempt, inference_error,
    invalid_response, passthrough, read_chunked_body, read_http_head, read_http_response,
    read_remaining_body, transfer_encoding, upstream_transport,
};
use crate::{
    inference::{InferenceAttempt, InferenceCancellation, InferenceResponse},
    router::{
        adapters::{
            AdapterError, AdapterResponse, AdapterTransportError as TransportError,
            UpstreamResponse,
        },
        cancellation::RequestCancellation,
        openai_compatible_codec as codec,
    },
};

/// Makes one chat attempt on the engine listening on `port`, which serves the model as
/// `served_model_id`, failing the rejections `classify` recognizes as invalid requests.
pub(super) async fn attempt(
    port: u16,
    served_model_id: &str,
    request: ChatRequest,
    classify: Option<RejectionClassifier>,
    cancellation: InferenceCancellation,
) -> InferenceAttempt {
    inference_attempt(
        invoke_loopback(port, served_model_id, request, classify, cancellation)
            .await
            .and_then(inference_response),
    )
}

#[tracing::instrument(level = "debug", skip_all, fields(operation = "inference_upstream"))]
async fn invoke_loopback(
    port: u16,
    served_model_id: &str,
    mut request: ChatRequest,
    classify: Option<RejectionClassifier>,
    mut cancellation: RequestCancellation,
) -> Result<AdapterResponse, AdapterError> {
    request.messages = instruction_roles::fold(std::mem::take(&mut request.messages))
        .map_err(AdapterError::InvalidRequest)?;
    // The engine serves its own model id; responses keep the request's public route name.
    let body = codec::encode_request_for_local_engine(&request, served_model_id)
        .map_err(AdapterError::InvalidRequest)?;
    let mut stream = http::post(
        port,
        "/v1/chat/completions",
        "application/json",
        &body,
        &mut cancellation,
    )
    .await?;
    if request.stream.unwrap_or(false) {
        return read_stream(stream, request, classify, cancellation).await;
    }
    let (status, headers, body) = read_http_response(&mut stream, &mut cancellation).await?;
    let upstream = UpstreamResponse {
        status,
        headers,
        body,
    };
    if !(200..300).contains(&upstream.status) {
        return unsuccessful(upstream, classify);
    }
    codec::decode_completion(&upstream.body)
        .map_err(|error| upstream_transport(TransportError::CompletionDecode(error)))
        .map(|mut completion| {
            completion.model_id = request.model_id;
            AdapterResponse::Complete(completion)
        })
}

/// Fails an unsuccessful engine answer that `classify` recognizes as an invalid request; any
/// other passes through unchanged.
fn unsuccessful(
    upstream: UpstreamResponse,
    classify: Option<RejectionClassifier>,
) -> Result<AdapterResponse, AdapterError> {
    if let Some(invalid) = classify.and_then(|classify| classify(upstream.status, &upstream.body)) {
        debug!(
            status = upstream.status,
            category = %invalid.category,
            "Engine chat rejection classified as an invalid request"
        );
        return Err(AdapterError::InvalidRequest(invalid));
    }
    Ok(AdapterResponse::Upstream(upstream))
}

/// Converts a chat transport answer into the engine-neutral response; a decision result is not a
/// chat answer, so it fails as an invalid engine response.
fn inference_response(response: AdapterResponse) -> Result<InferenceResponse, AdapterError> {
    Ok(match response {
        AdapterResponse::Complete(response) => InferenceResponse::Complete(response),
        AdapterResponse::Decision(_) => {
            return Err(invalid_response(
                "chat transport answered with a decision result",
            ));
        }
        AdapterResponse::Stream(mut stream) => {
            let (sender, receiver) = mpsc::channel(8);
            tokio::spawn(async move {
                while let Some(event) = stream.recv().await {
                    if sender.send(event.map_err(inference_error)).await.is_err() {
                        return;
                    }
                }
            });
            InferenceResponse::Stream(receiver)
        }
        AdapterResponse::Upstream(response) => {
            InferenceResponse::Passthrough(passthrough(response))
        }
    })
}

#[cfg(test)]
const TEST_SERVED_MODEL_ID: &str = "served-model";

#[cfg(test)]
async fn invoke(
    port: u16,
    request: ChatRequest,
    cancellation: RequestCancellation,
) -> Result<AdapterResponse, AdapterError> {
    invoke_loopback(port, TEST_SERVED_MODEL_ID, request, None, cancellation).await
}

async fn read_stream(
    mut stream: TcpStream,
    request: ChatRequest,
    classify: Option<RejectionClassifier>,
    mut cancellation: RequestCancellation,
) -> Result<AdapterResponse, AdapterError> {
    let (status, headers, pending) = read_http_head(&mut stream, &mut cancellation).await?;
    if !(200..300).contains(&status) {
        let mut pending = pending;
        if transfer_encoding(&headers)?.is_some() {
            pending = read_chunked_body(&mut stream, &mut cancellation, pending).await?;
        } else {
            read_remaining_body(&mut stream, &mut cancellation, &mut pending).await?;
        }
        return unsuccessful(
            UpstreamResponse {
                status,
                headers,
                body: pending,
            },
            classify,
        );
    }
    let (pending, chunked) = if transfer_encoding(&headers)?.is_some() {
        let mut decoder = ChunkedBodyDecoder::new();
        let pending = decoder.push(&pending)?;
        (pending, Some(decoder))
    } else {
        (pending, None)
    };
    let (sender, receiver) = mpsc::channel(8);
    let stream_span = debug_span!(
        "inference_upstream_stream",
        operation = "inference_upstream_stream",
        outcome = field::Empty,
        error_kind = field::Empty,
    );
    let record_span = stream_span.clone();
    tokio::spawn(
        async move {
            let result = forward_stream(
                &mut stream,
                &request,
                &mut cancellation,
                pending,
                chunked,
                &sender,
            )
            .await;
            match result {
                Ok(()) => {
                    record_span.record("outcome", "success");
                }
                Err(error) => {
                    record_span.record("outcome", "failed");
                    if matches!(error, AdapterError::Cancelled(_)) {
                        let _ = sender.try_send(Err(error));
                    } else {
                        let _ = sender.send(Err(error)).await;
                    }
                }
            }
        }
        .instrument(stream_span),
    );
    Ok(AdapterResponse::Stream(receiver))
}

async fn forward_stream(
    stream: &mut TcpStream,
    request: &ChatRequest,
    cancellation: &mut RequestCancellation,
    mut pending: Vec<u8>,
    mut chunked: Option<ChunkedBodyDecoder>,
    sender: &mpsc::Sender<Result<ChatStreamEvent, AdapterError>>,
) -> Result<(), AdapterError> {
    const MAX_UPSTREAM_STREAM_BUFFER_BYTES: usize = 1024 * 1024;
    let mut done = false;
    let include_usage = request.include_stream_usage.unwrap_or(false);
    let mut validator = codec::StreamValidator::new(include_usage);
    loop {
        while !done {
            let Some((end, separator_len)) = sse_frame_end(&pending) else {
                break;
            };
            let frame = String::from_utf8_lossy(&pending[..end]).replace('\r', "");
            pending.drain(..end + separator_len);
            let Some(data) = frame
                .lines()
                .find_map(|line| line.strip_prefix("data:").map(str::trim_start))
            else {
                continue;
            };
            if data == "[DONE]" {
                done = true;
                break;
            }
            for mut event in codec::decode_sse_frame(data)
                .map_err(|error| upstream_transport(TransportError::StreamDecode(error)))?
            {
                if validator
                    .apply(&event)
                    .map_err(|error| upstream_transport(TransportError::StreamPolicy(error)))?
                    == codec::StreamAction::Filter
                {
                    continue;
                }
                if let Some(Event::Started(started)) = &mut event.event {
                    started.model_id = request.model_id.clone();
                }
                tokio::select! {
                    biased;
                    reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
                    result = sender.send(Ok(event)) => {
                        if result.is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        }
        if done {
            if pending.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Err(invalid_response(
                    "OpenAI-compatible stream emitted a non-empty frame after [DONE]",
                ));
            }
            pending.clear();
            if chunked
                .as_ref()
                .is_some_and(ChunkedBodyDecoder::is_finished)
            {
                break;
            }
        }
        let mut buffer = [0; 8192];
        let read = tokio::select! {
            biased;
            reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
            value = stream.read(&mut buffer) => value.map_err(|error| AdapterError::Upstream(error.into()))?,
        };
        if read == 0 {
            break;
        }
        if let Some(decoder) = &mut chunked {
            pending.extend_from_slice(&decoder.push(&buffer[..read])?);
        } else {
            pending.extend_from_slice(&buffer[..read]);
        }
        if pending.len() > MAX_UPSTREAM_STREAM_BUFFER_BYTES {
            return Err(upstream_transport(TransportError::Limit(
                "OpenAI-compatible stream frame exceeds the size limit",
            )));
        }
    }
    if let Some(decoder) = &chunked {
        decoder.finish()?;
    }
    validator
        .finish(done, include_usage)
        .map_err(|error| upstream_transport(TransportError::StreamPolicy(error)))?;
    let completed = ChatStreamEvent {
        event: Some(Event::Completed(ChatStreamCompleted {})),
    };
    tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        _ = sender.send(Ok(completed)) => {}
    }
    Ok(())
}

fn sse_frame_end(pending: &[u8]) -> Option<(usize, usize)> {
    pending
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|end| (end, 2))
        .or_else(|| {
            pending
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|end| (end, 4))
        })
}

#[cfg(test)]
mod tests {
    use std::{io, sync::Arc};

    use dotcore_proto::dotcore::inference::v1::{FunctionToolDefinition, chat_stream_event::Event};
    use tokio::{io::AsyncWriteExt, sync::watch};

    use super::*;
    use crate::inference::{InferenceAttemptDisposition, InferenceError};
    use crate::test_support::http::read_request;
    use crate::{
        model::controller::serving_targets::ServingTargetsRevocationReason,
        router::{
            adapters::{AdapterAttempt, AdapterAttemptDisposition},
            cancellation::RequestCancellationReason,
            openai::OpenAiRequest,
        },
    };

    fn uncancelled() -> (
        RequestCancellation,
        watch::Sender<Option<RequestCancellationReason>>,
        watch::Sender<Option<ServingTargetsRevocationReason>>,
    ) {
        let (client, client_receiver) = watch::channel(None);
        let (route, route_receiver) = watch::channel(None);
        (
            RequestCancellation::new(client_receiver, route_receiver),
            client,
            route,
        )
    }

    async fn collect_events(
        mut events: mpsc::Receiver<Result<ChatStreamEvent, AdapterError>>,
    ) -> Vec<ChatStreamEvent> {
        let mut collected = Vec::new();
        while let Some(event) = events.recv().await {
            collected.push(event.expect("adapter event"));
        }
        collected
    }

    #[test]
    fn request_omits_unspecified_tool_properties() {
        let mut request = request(false);
        request.tools = vec![FunctionToolDefinition {
            name: "read_file".to_owned(),
            parameters_json_schema: r#"{"type":"object"}"#.to_owned(),
            description: None,
            strict: None,
        }];

        let encoded_request = serde_json::from_slice::<Value>(
            &codec::encode_request(&request).expect("valid request"),
        )
        .expect("request JSON");
        let function = encoded_request
            .pointer("/tools/0/function")
            .and_then(Value::as_object)
            .expect("OpenAI-compatible function tool");

        assert_eq!(function.get("name"), Some(&json!("read_file")));
        assert_eq!(function.get("parameters"), Some(&json!({"type":"object"})));
        assert!(!function.contains_key("description"));
        assert!(!function.contains_key("strict"));
    }

    #[tokio::test]
    async fn stream_always_requests_and_forwards_usage() {
        for client_intent in [None, Some(false), Some(true)] {
            assert_stream_usage(client_intent).await;
        }
    }

    #[tokio::test]
    async fn engine_receives_one_leading_system_message_and_replayable_reasoning() {
        let message = |role: ChatMessageRole, content: &str| ChatMessage {
            role: role as i32,
            content: Some(chat_message::Content::TextContent(content.to_owned())),
            ..Default::default()
        };
        let mut chat = request(false);
        chat.messages = vec![
            message(ChatMessageRole::System, "You are Codex."),
            message(ChatMessageRole::Developer, "Sandbox: read-only."),
            message(ChatMessageRole::User, "hi"),
            ChatMessage {
                reasoning: Some("Say hello.".to_owned()),
                ..message(ChatMessageRole::Assistant, "hello")
            },
            message(ChatMessageRole::Developer, "Be brief."),
            message(ChatMessageRole::User, "again"),
        ];
        let response: &'static [u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}";

        let (_, body) = invoke_capturing_request(chat, response).await;

        assert_eq!(
            serde_json::from_slice::<Value>(&body).expect("JSON request body")["messages"],
            json!([
                {"role":"system","content":"You are Codex.\n\nSandbox: read-only."},
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"hello","reasoning":"Say hello.","reasoning_content":"Say hello."},
                {"role":"user","content":"Be brief."},
                {"role":"user","content":"again"}
            ])
        );
    }

    #[tokio::test]
    async fn non_streamed_request_never_carries_stream_options() {
        for client_intent in [None, Some(false), Some(true)] {
            let mut chat = request(false);
            chat.include_stream_usage = client_intent;
            let response: &'static [u8] =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}";

            let (_, body) = invoke_capturing_request(chat, response).await;

            let sent = serde_json::from_slice::<Value>(&body).expect("JSON request body");
            assert_eq!(sent["stream"], false);
            assert!(
                sent.get("stream_options").is_none(),
                "client intent {client_intent:?} must not reach a non-streamed body"
            );
        }
    }

    #[tokio::test]
    async fn chunked_upstream_errors_are_decoded_for_pass_through() {
        for (status, body) in [
            (429, br#"{"error":"rate limited"}"#.as_slice()),
            (500, br#"{"error":"internal"}"#.as_slice()),
            (503, br#"{"error":"unavailable"}"#.as_slice()),
        ] {
            let response = invoke_chunked_error(status, body).await;
            assert_eq!(response.status, status);
            assert_eq!(response.body, body);
            assert!(response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("transfer-encoding") && value == "chunked"
            }));
            assert!(
                response
                    .headers
                    .iter()
                    .any(|(name, value)| name.eq_ignore_ascii_case("retry-after") && value == "7")
            );
            assert!(response.headers.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case("x-ratelimit-remaining") && value == "0"
            }));
        }
    }

    #[tokio::test]
    async fn ninfer_controls_reach_the_local_engine_and_its_null_usage_stream_decodes() {
        let OpenAiRequest::Chat(mut request) = crate::router::openai::decode(
            "POST",
            "/v1/chat/completions",
            crate::router::test_fixtures::PI_CHAT_REQUEST.as_bytes(),
        )
        .expect("Pi request") else {
            panic!("Pi fixture must decode as Chat")
        };
        let controls = crate::router::test_fixtures::PI_CHAT_EXTENSIONS;
        request.request_extensions_json = Some(format!(
            r#"{},"z_above_u64":184467440737095516160}}"#,
            &controls[..controls.len() - 1]
        ));
        let response: &'static [u8] = Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{}",
                crate::router::test_fixtures::NINFER_TEXT_STREAM
            )
            .into_bytes()
            .into_boxed_slice(),
        );

        let (result, body) = invoke_capturing_request(request, response).await;

        let text = String::from_utf8(body).expect("UTF-8 request body");
        let sent = serde_json::from_str::<Value>(&text).expect("JSON request body");
        assert_eq!(sent["model"], TEST_SERVED_MODEL_ID);
        assert_eq!(sent["store"], false);
        assert_eq!(sent["reasoning_effort"], "none");
        assert_eq!(
            sent["stream_options"],
            serde_json::json!({"include_usage": true})
        );
        assert_eq!(sent["top_k"], 20);
        assert_eq!(sent["repetition_penalty"], 1.0);
        assert_eq!(sent["presence_penalty"], 0.0);
        assert_eq!(
            sent["chat_template_kwargs"],
            serde_json::json!({"enable_thinking": false, "preserve_thinking": true, "reasoning_effort": "low", "stops": [null, "</think>"]})
        );
        assert!(text.contains(r#""seed":18446744073709551615"#));
        assert!(text.contains(r#""z_above_u64":184467440737095516160"#));

        let Ok(AdapterResponse::Stream(mut events)) = result else {
            panic!("stream response");
        };
        let mut kinds = Vec::new();
        while let Some(event) = events.recv().await {
            kinds.push(match event.expect("valid ninfer event").event {
                Some(Event::Started(started)) => format!("started:{}", started.model_id),
                Some(Event::TextDelta(_)) => "text".to_owned(),
                Some(Event::Finished(_)) => "finished".to_owned(),
                Some(Event::Usage(usage)) => format!("usage:{}", usage.total_tokens),
                Some(Event::Completed(_)) => "completed".to_owned(),
                other => panic!("unexpected event {other:?}"),
            });
        }
        assert_eq!(
            kinds,
            [
                "started:qwen3-8-27b",
                "text",
                "text",
                "finished",
                "usage:14",
                "completed"
            ]
        );
    }

    #[tokio::test]
    async fn an_invalid_request_fails_before_contacting_the_engine_and_never_falls_back() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();
        let invoker = super::super::loopback_http::LoopbackInvoker::bind(
            port,
            TEST_SERVED_MODEL_ID.to_owned(),
            &[dotcore_proto::dotcore::catalog::v1::InferenceRequestKind::Chat],
            Default::default(),
        );
        let mut request = request(false);
        request.request_extensions_json = Some(r#"{"stream":true}"#.to_owned());

        let attempt = invoker
            .invoke(request, crate::inference::inference_cancellation().1)
            .await;

        assert!(matches!(
            attempt,
            InferenceAttempt::Failed {
                disposition: InferenceAttemptDisposition::NonRetryable,
                error: InferenceError::InvalidRequest(error),
            } if error == crate::inference::InvalidRequest::invalid_field(Some("stream"))
        ));
    }

    #[tokio::test]
    async fn refused_loopback_connection_is_the_only_safe_fallback_attempt() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();
        let (cancellation, _client, _route) = uncancelled();

        let attempt = AdapterAttempt::from_result(invoke(port, request(false), cancellation).await);

        let AdapterAttempt::Failed {
            disposition: AdapterAttemptDisposition::SafeToFallback,
            error: AdapterError::PreDispatchUnavailable(source),
        } = &attempt
        else {
            panic!("refused connection must remain a safe pre-dispatch failure");
        };
        let Some(TransportError::Connect(error)) = source.downcast_ref() else {
            panic!("refused connection must retain its transport source");
        };
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert!(attempt.allows_fallback());
    }

    #[tokio::test]
    async fn engine_receives_its_served_model_while_the_completion_keeps_the_route_name() {
        let (result, body) = invoke_capturing(
            false,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 174\r\nConnection: close\r\n\r\n{\"id\":\"chatcmpl_1\",\"object\":\"chat.completion\",\"created\":0,\"model\":\"served-model\",\"choices\":[{\"index\":0,\"message\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}",
        )
        .await;

        assert_eq!(body["model"], TEST_SERVED_MODEL_ID);
        let Ok(AdapterResponse::Complete(completion)) = result else {
            panic!("completion response");
        };
        assert_eq!(completion.model_id, "requested-model");
    }

    #[tokio::test]
    async fn streaming_engine_receives_its_served_model_while_events_keep_the_route_name() {
        let (result, body) = invoke_capturing(
            true,
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"chatcmpl_1\",\"model\":\"served-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        )
        .await;

        assert_eq!(body["model"], TEST_SERVED_MODEL_ID);
        let Ok(AdapterResponse::Stream(mut events)) = result else {
            panic!("stream response");
        };
        let started = events
            .recv()
            .await
            .expect("started event")
            .expect("valid started event");
        assert!(matches!(
            started.event,
            Some(Event::Started(started)) if started.model_id == "requested-model"
        ));
    }

    #[tokio::test]
    async fn rejected_served_model_passes_the_engine_response_through() {
        let (result, body) = invoke_capturing(
            false,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;

        assert_eq!(body["model"], TEST_SERVED_MODEL_ID);
        let Ok(AdapterResponse::Upstream(upstream)) = result else {
            panic!("passthrough response");
        };
        assert_eq!(upstream.status, 404);
    }

    #[test]
    fn non_refused_connection_failure_never_allows_fallback() {
        let attempt = AdapterAttempt::from_result(Err(upstream_transport(
            TransportError::Connect(io::Error::from(io::ErrorKind::TimedOut)),
        )));

        assert_no_fallback(&attempt);
    }

    #[test]
    fn successful_connect_write_failure_never_allows_fallback() {
        let attempt = AdapterAttempt::from_result(Err(upstream_transport(TransportError::Io(
            io::Error::from(io::ErrorKind::BrokenPipe),
        ))));

        assert_no_fallback(&attempt);
    }

    #[tokio::test]
    async fn timeout_after_a_successful_connect_never_allows_fallback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let connected = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let upstream = tokio::spawn({
            let connected = connected.clone();
            let release = release.clone();
            async move {
                let (_stream, _) = listener.accept().await.expect("upstream connection");
                connected.wait().await;
                release.wait().await;
            }
        });
        let (cancellation, client, _route) = uncancelled();
        let invoke = tokio::spawn(invoke(port, request(false), cancellation));
        connected.wait().await;
        client.send_replace(Some(RequestCancellationReason::RequestTimedOut));
        let attempt = AdapterAttempt::from_result(invoke.await.expect("adapter task"));

        assert!(matches!(
            &attempt,
            AdapterAttempt::Failed {
                error: AdapterError::Cancelled(RequestCancellationReason::RequestTimedOut),
                ..
            }
        ));
        assert_no_fallback(&attempt);
        release.wait().await;
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn eof_after_a_successful_connect_never_allows_fallback() {
        let attempt = invoke_after_request(b"").await;

        assert_no_fallback(&attempt);
    }

    #[tokio::test]
    async fn malformed_response_after_a_successful_connect_never_allows_fallback() {
        let attempt = invoke_after_request(b"HTTP/1.1 invalid\r\n\r\n").await;

        assert_no_fallback(&attempt);
    }

    #[tokio::test]
    async fn http_5xx_after_a_successful_connect_never_allows_fallback() {
        let attempt = invoke_after_request(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;

        assert!(matches!(
            &attempt,
            AdapterAttempt::Response(AdapterResponse::Upstream(UpstreamResponse {
                status: 503,
                ..
            }))
        ));
        assert_no_fallback(&attempt);
    }

    #[tokio::test]
    async fn stream_failure_after_reasoning_never_allows_fallback() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"chatcmpl_first\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"reasoning_content\":\"first\"},\"finish_reason\":null}]}\n\ndata: not-json\n\n")
                .await
                .expect("upstream stream");
        });
        let (cancellation, _client, _route) = uncancelled();
        let attempt = AdapterAttempt::from_result(invoke(port, request(true), cancellation).await);

        assert_no_fallback(&attempt);
        let AdapterAttempt::Response(AdapterResponse::Stream(mut events)) = attempt else {
            panic!("stream must begin before its malformed second event");
        };
        assert!(matches!(
            events.recv().await,
            Some(Ok(ChatStreamEvent {
                event: Some(Event::Started(_)),
            }))
        ));
        let mut reasoning_emitted = false;
        let failure = loop {
            match events.recv().await {
                Some(Ok(ChatStreamEvent {
                    event: Some(Event::ReasoningDelta(delta)),
                })) if delta.choice_index == 0 && delta.reasoning == "first" => {
                    reasoning_emitted = true;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => break error,
                None => panic!("malformed stream must report an error"),
            }
        };
        assert!(reasoning_emitted);
        assert!(matches!(failure, AdapterError::Upstream(_)));
        upstream.await.expect("upstream task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn successful_chunked_sse_is_decoded_incrementally() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let first_sent = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let upstream = tokio::spawn({
            let first_sent = first_sent.clone();
            let release = release.clone();
            async move {
                let (mut stream, _) = listener.accept().await.expect("upstream connection");
                read_request(&mut stream).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("upstream headers");
                let first = b"data: {\"id\":\"chatcmpl_chunked\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"first\"},\"finish_reason\":null}]}\n\n";
                stream.write_all(&chunk(first)).await.expect("first chunk");
                first_sent.wait().await;
                release.wait().await;
                let terminal = b"data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\ndata: [DONE]\n\n";
                stream
                    .write_all(&chunk(terminal))
                    .await
                    .expect("terminal chunk");
                stream
                    .write_all(b"0\r\n\r\n")
                    .await
                    .expect("chunk terminator");
            }
        });
        let (cancellation, _client, _route) = uncancelled();
        let mut chat = request(true);
        chat.include_stream_usage = Some(true);
        let response = invoke(port, chat, cancellation)
            .await
            .expect("chunked stream");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("stream response")
        };
        first_sent.wait().await;
        let started = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .expect("incremental event")
            .expect("started event")
            .expect("valid started event");
        assert!(matches!(started.event, Some(Event::Started(_))));
        release.wait().await;
        let mut remaining = Vec::new();
        while let Some(event) = events.recv().await {
            remaining.push(event.expect("valid stream event"));
        }
        assert!(
            remaining
                .iter()
                .any(|event| matches!(event.event, Some(Event::Usage(_))))
        );
        assert!(matches!(
            remaining.last().and_then(|event| event.event.as_ref()),
            Some(Event::Completed(_))
        ));
        upstream.await.expect("upstream task");
    }

    #[tokio::test]
    async fn stream_rejects_deltas_before_started() {
        let error = invalid_stream_error(
            b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"invalid\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            false,
        )
        .await;
        assert!(error.to_string().contains("before its Started event"));
    }

    #[tokio::test]
    async fn stream_rejects_missing_requested_terminal_usage() {
        let error = invalid_stream_error(
            b"data: {\"id\":\"chatcmpl_missing_usage\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            true,
        )
        .await;
        assert!(error.to_string().contains("requested terminal usage"));
    }

    #[tokio::test]
    async fn stream_rejects_every_choice_event_after_that_choice_is_finished() {
        for trailing_frame in [
            r#"{"choices":[{"index":0,"delta":{"content":"late"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_late","type":"function","function":{"name":"late","arguments":"{}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
        ] {
            let body = format!(
                "data: {{\"id\":\"chatcmpl_finished\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"done\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: {trailing_frame}\n\ndata: [DONE]\n\n"
            );
            let error =
                invalid_stream_error(Box::leak(body.into_bytes().into_boxed_slice()), false).await;
            assert!(error.to_string().contains("after its Finished event"));
        }
    }

    #[tokio::test]
    async fn stream_rejects_a_frame_fragmented_across_chunks_after_done() {
        let terminal = b"data: {\"id\":\"chatcmpl_done\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let trailing = b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n";
        let error = invalid_chunked_stream_error(&[
            terminal.as_slice(),
            &trailing[..7],
            &trailing[7..31],
            &trailing[31..],
        ])
        .await;

        assert!(error.to_string().contains("after [DONE]"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn route_revocation_before_connect_returns_typed_cancellation() {
        let (cancellation, _client, route) = uncancelled();
        let start = Arc::new(tokio::sync::Barrier::new(2));
        let invoke = tokio::spawn({
            let start = start.clone();
            async move {
                start.wait().await;
                invoke(9, request(false), cancellation).await
            }
        });
        route.send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));
        start.wait().await;

        assert!(matches!(
            invoke.await.expect("adapter task"),
            Err(AdapterError::Cancelled(
                RequestCancellationReason::RouteRevoked
            ))
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_cancellation_stops_a_pending_non_stream_response() {
        assert_pending_non_stream_cancellation(RequestCancellationReason::ClientDisconnected).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn request_timeout_stops_a_pending_non_stream_response() {
        assert_pending_non_stream_cancellation(RequestCancellationReason::RequestTimedOut).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn route_revocation_stops_a_pending_non_stream_response() {
        assert_pending_non_stream_cancellation(RequestCancellationReason::RouteRevoked).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn route_revocation_stops_incremental_stream_forwarding() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let frame_sent = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let upstream = tokio::spawn({
            let frame_sent = frame_sent.clone();
            let release = release.clone();
            async move {
                let (mut stream, _) = listener.accept().await.expect("upstream connection");
                read_request(&mut stream).await;
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"chatcmpl_upstream\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"one\"},\"finish_reason\":null}]}\n\n").await.expect("upstream frame");
                frame_sent.wait().await;
                release.wait().await;
            }
        });
        let (cancellation, _client, route) = uncancelled();
        let response = invoke(port, request(true), cancellation)
            .await
            .expect("stream response");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("stream response")
        };
        frame_sent.wait().await;
        let started = events
            .recv()
            .await
            .expect("started event")
            .expect("valid started event");
        assert!(matches!(
            started.event,
            Some(Event::Started(started)) if started.model_id == "requested-model"
        ));
        route.send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));

        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                if let Some(Err(error)) = events.recv().await {
                    break error;
                }
            }
        })
        .await
        .expect("stream cancellation");
        assert!(matches!(
            cancelled,
            AdapterError::Cancelled(RequestCancellationReason::RouteRevoked)
        ));
        release.wait().await;
        upstream.await.expect("upstream task");
    }

    async fn assert_pending_non_stream_cancellation(reason: RequestCancellationReason) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let connected = Arc::new(tokio::sync::Barrier::new(2));
        let release = Arc::new(tokio::sync::Barrier::new(2));
        let upstream = tokio::spawn({
            let connected = connected.clone();
            let release = release.clone();
            async move {
                let (_stream, _) = listener.accept().await.expect("upstream connection");
                connected.wait().await;
                release.wait().await;
            }
        });
        let (cancellation, client, route) = uncancelled();
        let invoke = tokio::spawn(invoke(port, request(false), cancellation));
        connected.wait().await;
        match reason {
            RequestCancellationReason::ClientDisconnected => {
                client.send_replace(Some(reason));
            }
            RequestCancellationReason::RequestTimedOut => {
                client.send_replace(Some(reason));
            }
            RequestCancellationReason::RouteRevoked => {
                route.send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));
            }
        }
        assert!(matches!(
            invoke.await.expect("adapter task"),
            Err(AdapterError::Cancelled(actual)) if actual == reason
        ));
        release.wait().await;
        upstream.await.expect("upstream task");
    }

    async fn assert_stream_usage(client_intent: Option<bool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let body = read_request(&mut stream).await;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"id\":\"chatcmpl_upstream\",\"model\":\"upstream-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":1,\"total_tokens\":3}}\n\ndata: [DONE]\n\n").await.expect("upstream stream");
            body
        });
        let (cancellation, _client, _route) = uncancelled();
        let mut chat = request(true);
        chat.include_stream_usage = client_intent;
        let response = invoke(port, chat, cancellation)
            .await
            .expect("OpenAI-compatible stream");
        let AdapterResponse::Stream(events) = response else {
            panic!("stream response")
        };
        let events = collect_events(events).await;
        let usage = events
            .iter()
            .filter_map(|event| match &event.event {
                Some(Event::Usage(usage)) => Some((
                    usage.prompt_tokens,
                    usage.completion_tokens,
                    usage.total_tokens,
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(usage, [(2, 1, 3)], "client intent {client_intent:?}");
        assert!(matches!(
            events.last().and_then(|event| event.event.as_ref()),
            Some(Event::Completed(_))
        ));
        let sent = serde_json::from_slice::<Value>(&upstream.await.expect("upstream task"))
            .expect("JSON request body");
        assert_eq!(
            sent["stream_options"],
            serde_json::json!({"include_usage": true}),
            "client intent {client_intent:?}"
        );
    }

    async fn invoke_chunked_error(status: u16, body: &[u8]) -> UpstreamResponse {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let first = body.len() / 2;
        let encoded = format!(
            "{:x}\r\n{}\r\n{:x};fixture=yes\r\n{}\r\n0\r\n\r\n",
            first,
            std::str::from_utf8(&body[..first]).expect("first chunk"),
            body.len() - first,
            std::str::from_utf8(&body[first..]).expect("second chunk")
        );
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            read_request(&mut stream).await;
            let headers = format!(
                "HTTP/1.1 {status} Upstream Error\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nRetry-After: 7\r\nX-RateLimit-Remaining: 0\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .await
                .expect("upstream headers");
            stream
                .write_all(encoded.as_bytes())
                .await
                .expect("upstream chunks");
        });
        let (cancellation, _client, _route) = uncancelled();
        let response = invoke(port, request(false), cancellation)
            .await
            .expect("chunked upstream response");
        upstream.await.expect("upstream task");
        let AdapterResponse::Upstream(response) = response else {
            panic!("upstream pass-through response")
        };
        response
    }

    async fn invalid_stream_error(body: &'static [u8], include_usage: bool) -> AdapterError {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .expect("upstream headers");
            stream.write_all(body).await.expect("upstream body");
        });
        let (cancellation, _client, _route) = uncancelled();
        let mut chat = request(true);
        chat.include_stream_usage = Some(include_usage);
        let response = invoke(port, chat, cancellation)
            .await
            .expect("stream response");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("stream response")
        };
        let error = loop {
            match events.recv().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => {}
                None => panic!("invalid stream must report an error"),
            }
        };
        upstream.await.expect("upstream task");
        error
    }

    async fn invalid_chunked_stream_error(chunks: &[&[u8]]) -> AdapterError {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let body = chunks.iter().fold(Vec::new(), |mut body, data| {
            body.extend_from_slice(&chunk(data));
            body
        });
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                .await
                .expect("upstream headers");
            stream.write_all(&body).await.expect("upstream chunks");
            stream
                .write_all(b"0\r\n\r\n")
                .await
                .expect("chunk terminator");
        });
        let (cancellation, _client, _route) = uncancelled();
        let response = invoke(port, request(true), cancellation)
            .await
            .expect("stream response");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("stream response")
        };
        let error = loop {
            match events.recv().await {
                Some(Err(error)) => break error,
                Some(Ok(_)) => {}
                None => panic!("invalid stream must report an error"),
            }
        };
        upstream.await.expect("upstream task");
        error
    }

    fn assert_no_fallback(attempt: &AdapterAttempt) {
        assert!(
            !attempt.allows_fallback(),
            "a started or ambiguous adapter attempt must not try a fallback target"
        );
    }

    async fn invoke_after_request(response: &'static [u8]) -> AdapterAttempt {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            read_request(&mut stream).await;
            if !response.is_empty() {
                stream.write_all(response).await.expect("upstream response");
            }
        });
        let (cancellation, _client, _route) = uncancelled();
        let attempt = AdapterAttempt::from_result(invoke(port, request(false), cancellation).await);
        upstream.await.expect("upstream task");
        attempt
    }

    /// Invokes a test upstream and returns the transport result with the JSON body it received.
    async fn invoke_capturing(
        stream: bool,
        response: &'static [u8],
    ) -> (Result<AdapterResponse, AdapterError>, serde_json::Value) {
        let (result, body) = invoke_capturing_request(request(stream), response).await;
        (
            result,
            serde_json::from_slice(&body).expect("JSON request body"),
        )
    }

    /// Invokes a test upstream with `request` and returns the raw request body it received.
    async fn invoke_capturing_request(
        request: ChatRequest,
        response: &'static [u8],
    ) -> (Result<AdapterResponse, AdapterError>, Vec<u8>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.expect("upstream connection");
            let body = read_request(&mut connection).await;
            connection
                .write_all(response)
                .await
                .expect("upstream response");
            body
        });
        let (cancellation, _client, _route) = uncancelled();
        let result = invoke(port, request, cancellation).await;
        (result, upstream.await.expect("upstream task"))
    }

    fn chunk(body: &[u8]) -> Vec<u8> {
        let mut chunk = format!("{:x}\r\n", body.len()).into_bytes();
        chunk.extend_from_slice(body);
        chunk.extend_from_slice(b"\r\n");
        chunk
    }

    fn request(stream: bool) -> ChatRequest {
        ChatRequest {
            model_id: "requested-model".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content: Some(chat_message::Content::TextContent("hello".to_owned())),
                ..Default::default()
            }],
            stream: Some(stream),
            ..Default::default()
        }
    }
}
