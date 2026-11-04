//! HTTP parsing, response-byte, and SSE compatibility tests.

use std::{sync::Arc, time::Duration};

use dotcore_domain::test_support::tracing::{Record, capture_records, capture_records_async};
use dotcore_observability::{MetricRecord, MetricRecorder, RecordValue, labels::LabelSet};
use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;
use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionChoice, ChatCompletionResult, ChatStreamEvent, ChatStreamStarted, ChatTextDelta,
    TokenUsage, chat_stream_event::Event, chat_text_delta,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, watch},
};

use super::{
    connection::{
        ChatRoute, OpenAiRoute, answered_attempt_end, fallback_exhausted_response,
        log_fallback_exhausted, log_target_attempt, record_stream, record_written_response,
        selection_response, stream_attempt_end, write_response,
    },
    errors::OpenAiWriteError,
    request::parse_openai_request_head,
    response::{ResponseWrite, openai_response_bytes, write_openai_response_cancellable},
    stream::{StreamWriteOutcome, write_openai_stream, write_responses_stream},
};
use crate::{
    model::controller::serving_targets::ServingTargetsRevocationReason,
    router::{
        adapters::{AdapterAttempt, AdapterError, AdapterResponse},
        cancellation::{RequestCancellation, RequestCancellationReason},
        metrics::RequestOutcome,
        metrics::{
            ATTEMPT_DECODE_RATE, ATTEMPT_DURATION, ATTEMPT_PREFILL_RATE, ATTEMPT_TTFT, ATTEMPTS,
            REQUEST_DURATION, REQUEST_FALLBACKS, REQUEST_IN_FLIGHT, REQUEST_TTFT, REQUESTS,
            TOKENS_CACHED_PROMPT, TOKENS_COMPLETION, TOKENS_PROMPT, USAGE_MISSING,
        },
        peer_service::test_support::{DEPLOYMENT, Script, WORKER_NODE_ID},
        selection::TargetSelectionError,
    },
};

#[test]
fn two_target_fallback_attempts_are_distinct_and_do_not_mutate_request_context() {
    let (_, records, output) = capture_records(|| {
        let span = tracing::info_span!(
            "inference_request",
            model_id = "model",
            status = tracing::field::Empty,
        );
        log_target_attempt(&span, "route", "primary", 0, 0);
        log_target_attempt(&span, "route", "fallback", 1, 1);
        drop(span);
    });

    let attempts = records
        .iter()
        .filter_map(|record| match record {
            Record::Event { fields, .. }
                if fields.get("event_kind") == Some(&"\"inference_target_attempt\"".to_owned()) =>
            {
                Some(fields)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(attempts.len(), 2);
    assert_eq!(
        attempts[0].get("deployment_id"),
        Some(&"primary".to_owned())
    );
    assert_eq!(
        attempts[0].get("fallback_group_index"),
        Some(&"0".to_owned())
    );
    assert_eq!(attempts[0].get("attempt_index"), Some(&"0".to_owned()));
    assert_eq!(
        attempts[1].get("deployment_id"),
        Some(&"fallback".to_owned())
    );
    assert_eq!(
        attempts[1].get("fallback_group_index"),
        Some(&"1".to_owned())
    );
    assert_eq!(attempts[1].get("attempt_index"), Some(&"1".to_owned()));

    let close = output
        .lines()
        .find(|line| line.contains("close time.busy"))
        .expect("formatted request span close");
    assert!(close.contains("model_id=\"model\""));
    assert!(!close.contains("deployment_id"));
}

#[test]
fn fallback_exhaustion_emits_a_bounded_router_event() {
    let (_, records, _) = capture_records(|| {
        log_fallback_exhausted(
            &tracing::info_span!("inference_request"),
            "route",
            &TargetSelectionError::Unavailable,
        );
    });

    assert!(records.iter().any(|record| matches!(
        record,
        Record::Event { level, fields }
            if level == "WARN"
                && fields.get("event_kind") == Some(&"\"inference_fallback_exhausted\"".to_owned())
                && fields.get("route") == Some(&"route".to_owned())
                && fields.get("error_kind") == Some(&"\"target_unavailable\"".to_owned())
    )));
}

#[test]
fn request_parser_retains_bounded_body_validation() {
    let bytes = b"POST /v1/chat/completions HTTP/1.1\r\nContent-Length: 2\r\n\r\n{}".to_vec();
    let header_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("header terminator");
    let head = parse_openai_request_head(bytes, header_end).expect("request head");
    assert_eq!(head.method, "POST");
    assert_eq!(head.path, "/v1/chat/completions");
}

#[test]
fn response_bytes_map_upstream_failures_without_provider_data() {
    let bytes = openai_response_bytes(crate::router::openai::upstream(
        429,
        vec![
            ("Retry-After".into(), "7".into()),
            ("X-Private".into(), "secret".into()),
        ],
        br#"{\"error\":{\"message\":\"private endpoint body\"}}"#.to_vec(),
    ))
    .expect("response bytes");
    let response = String::from_utf8(bytes).expect("response UTF-8");
    assert!(response.starts_with("HTTP/1.1 429 Upstream Response\r\n"));
    assert!(response.contains("retry-after: 7\r\n"));
    assert!(response.contains("The inference engine rejected the request."));
    assert!(!response.contains("secret"));
    assert!(!response.contains("private endpoint body"));

    let server_error =
        openai_response_bytes(crate::router::openai::upstream(503, Vec::new(), Vec::new()))
            .expect("server error bytes");
    assert!(
        String::from_utf8(server_error)
            .expect("server error UTF-8")
            .starts_with("HTTP/1.1 502 Bad Gateway\r\n")
    );
}

#[tokio::test]
async fn stream_failure_before_a_valid_event_returns_a_bounded_upstream_error() {
    let (mut writer, mut client) = connected_pair().await;
    let (sender, events) = mpsc::channel(1);
    sender
        .send(Err(AdapterError::upstream("malformed upstream stream")))
        .await
        .expect("stream receiver");
    drop(sender);

    let outcome = write_openai_stream(
        &mut writer,
        events,
        uncancelled(),
        "route-name".to_owned(),
        true,
        &mut |_| {},
    )
    .await
    .expect("stream write");
    drop(writer);
    let mut public = Vec::new();
    client
        .read_to_end(&mut public)
        .await
        .expect("public stream read");

    assert!(matches!(
        outcome,
        StreamWriteOutcome::Failed {
            error_kind: "upstream",
            ..
        }
    ));
    let header_end = public
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .expect("response headers");
    assert_eq!(
        &public[..header_end + 4],
        b"HTTP/1.1 502 Bad Gateway\r\nContent-Type: application/json\r\nContent-Length: 123\r\nConnection: close\r\n\r\n"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&public[header_end + 4..])
            .expect("bounded OpenAI error JSON"),
        serde_json::json!({"error":{
            "message":"The inference engine returned an invalid response.",
            "type":"upstream_error",
            "param":null,
            "code":null
        }})
    );
}

#[tokio::test]
async fn chat_stream_reaches_http_client_before_source_completes() {
    let (mut writer, mut client) = connected_pair().await;
    let (sender, events) = mpsc::channel(2);
    let stream = tokio::spawn(async move {
        let outcome = write_openai_stream(
            &mut writer,
            events,
            uncancelled(),
            "route-name".to_owned(),
            true,
            &mut |_| {},
        )
        .await
        .expect("stream write");
        drop(writer);
        outcome
    });
    sender
        .send(Ok(ChatStreamEvent {
            event: Some(Event::Started(ChatStreamStarted {
                completion_id: "chatcmpl_first".to_owned(),
                model_id: "catalog-model".to_owned(),
                created_at_unix_seconds: 1,
            })),
        }))
        .await
        .expect("first stream event");

    let mut first = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !first
            .windows(b"chatcmpl_first".len())
            .any(|part| part == b"chatcmpl_first")
        {
            let mut buffer = [0; 1024];
            let size = client.read(&mut buffer).await.expect("first HTTP bytes");
            assert!(size > 0, "HTTP stream closed before its first event");
            first.extend_from_slice(&buffer[..size]);
        }
    })
    .await
    .expect("first event must arrive while the source remains open");
    let first = String::from_utf8(first).expect("UTF-8 SSE");
    assert!(first.starts_with("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n"));
    assert!(first.contains("\"model\":\"route-name\""));
    assert!(!first.contains("[DONE]"));

    sender
        .send(Ok(ChatStreamEvent {
            event: Some(Event::Completed(Default::default())),
        }))
        .await
        .expect("terminal stream event");
    drop(sender);
    let mut remaining = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.read_to_end(&mut remaining),
    )
    .await
    .expect("stream completion timeout")
    .expect("terminal HTTP bytes");
    assert!(
        String::from_utf8(remaining)
            .expect("UTF-8 SSE")
            .contains("data: [DONE]\n\n")
    );
    assert_eq!(
        stream.await.expect("stream task"),
        StreamWriteOutcome::Complete
    );
}

#[tokio::test]
async fn unrequested_usage_is_dropped_from_the_chat_stream_and_requested_usage_is_kept() {
    for (include_usage, usage_frame) in [
        (false, ""),
        (
            true,
            "data: {\"choices\":[],\"created\":1,\"id\":\"chatcmpl_u\",\"model\":\"route-name\",\"object\":\"chat.completion.chunk\",\"usage\":{\"completion_tokens\":1,\"prompt_tokens\":2,\"total_tokens\":3}}\n\n",
        ),
    ] {
        let (mut writer, mut client) = connected_pair().await;
        let (sender, events) = mpsc::channel(8);
        for event in [
            Event::Started(ChatStreamStarted {
                completion_id: "chatcmpl_u".to_owned(),
                model_id: "catalog-model".to_owned(),
                created_at_unix_seconds: 1,
            }),
            Event::TextDelta(ChatTextDelta {
                choice_index: 0,
                role: None,
                content: Some(chat_text_delta::Content::TextContent("hi".to_owned())),
            }),
            Event::Finished(Default::default()),
            Event::Usage(dotcore_proto::dotcore::inference::v1::TokenUsage {
                prompt_tokens: 2,
                completion_tokens: 1,
                total_tokens: 3,
                ..Default::default()
            }),
            Event::Completed(Default::default()),
        ] {
            sender
                .send(Ok(ChatStreamEvent { event: Some(event) }))
                .await
                .expect("stream receiver");
        }
        drop(sender);

        let outcome = write_openai_stream(
            &mut writer,
            events,
            uncancelled(),
            "route-name".to_owned(),
            include_usage,
            &mut |_| {},
        )
        .await
        .expect("stream write");
        drop(writer);
        let mut public = Vec::new();
        client
            .read_to_end(&mut public)
            .await
            .expect("public stream read");

        assert_eq!(outcome, StreamWriteOutcome::Complete);
        let body = String::from_utf8(public).expect("UTF-8 stream");
        let body = body.split_once("\r\n\r\n").expect("HTTP head").1;
        const CHUNKS: &str = "data: {\"choices\":[],\"created\":1,\"id\":\"chatcmpl_u\",\"model\":\"route-name\",\"object\":\"chat.completion.chunk\"}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"},\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"chatcmpl_u\",\"model\":\"route-name\",\"object\":\"chat.completion.chunk\"}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":null,\"index\":0}],\"created\":1,\"id\":\"chatcmpl_u\",\"model\":\"route-name\",\"object\":\"chat.completion.chunk\"}\n\n";
        assert_eq!(body, format!("{CHUNKS}{usage_frame}data: [DONE]\n\n"));
    }
}

#[tokio::test]
async fn stream_failure_after_the_first_valid_event_preserves_public_commitment() {
    let (mut writer, mut client) = connected_pair().await;
    let (sender, events) = mpsc::channel(2);
    sender
        .send(Ok(ChatStreamEvent {
            event: Some(Event::Started(ChatStreamStarted {
                completion_id: "chatcmpl_first".to_owned(),
                model_id: "catalog-model".to_owned(),
                created_at_unix_seconds: 1,
            })),
        }))
        .await
        .expect("stream receiver");
    sender
        .send(Err(AdapterError::upstream("malformed upstream stream")))
        .await
        .expect("stream receiver");
    drop(sender);

    let outcome = write_openai_stream(
        &mut writer,
        events,
        uncancelled(),
        "route-name".to_owned(),
        true,
        &mut |_| {},
    )
    .await
    .expect("stream write");
    drop(writer);
    let mut public = Vec::new();
    client
        .read_to_end(&mut public)
        .await
        .expect("public stream read");

    assert!(matches!(
        outcome,
        StreamWriteOutcome::Failed {
            error_kind: "upstream",
            ..
        }
    ));
    assert!(public.starts_with(b"HTTP/1.1 200 OK\r\n"));
    assert!(
        public
            .windows(b"chatcmpl_first".len())
            .any(|bytes| bytes == b"chatcmpl_first")
    );
    assert!(
        public
            .windows(b"\"model\":\"route-name\"".len())
            .any(|bytes| bytes == b"\"model\":\"route-name\"")
    );
    assert!(
        !public
            .windows(b"HTTP/1.1 502".len())
            .any(|bytes| bytes == b"HTTP/1.1 502")
    );
    assert!(!public.ends_with(b"data: [DONE]\n\n"));
}

#[tokio::test]
async fn a_second_responses_choice_fails_the_stream_before_or_after_commit() {
    let started = || {
        Ok(ChatStreamEvent {
            event: Some(Event::Started(ChatStreamStarted {
                completion_id: "chatcmpl_first".to_owned(),
                model_id: "engine-model".to_owned(),
                created_at_unix_seconds: 1,
            })),
        })
    };
    let second_choice = || {
        Ok(ChatStreamEvent {
            event: Some(Event::TextDelta(ChatTextDelta {
                choice_index: 1,
                role: None,
                content: Some(chat_text_delta::Content::TextContent(
                    "second-choice-text".to_owned(),
                )),
            })),
        })
    };
    for (events, committed) in [
        (vec![second_choice()], false),
        (vec![started(), second_choice()], true),
    ] {
        let (mut writer, mut client) = connected_pair().await;
        let (sender, receiver) = mpsc::channel(events.len());
        for event in events {
            sender.send(event).await.expect("stream receiver");
        }
        drop(sender);

        let outcome = write_responses_stream(
            &mut writer,
            receiver,
            uncancelled(),
            "route".to_owned(),
            &mut |_| {},
        )
        .await
        .expect("stream write");
        drop(writer);
        let mut public = Vec::new();
        client
            .read_to_end(&mut public)
            .await
            .expect("public stream read");

        assert_eq!(
            outcome,
            StreamWriteOutcome::Failed {
                error_kind: "upstream",
                failure_stage: "unexpected_choice",
                committed,
                status: if committed { 200 } else { 502 },
            }
        );
        let expected_status: &[u8] = if committed {
            b"HTTP/1.1 200 OK\r\n"
        } else {
            b"HTTP/1.1 502 Bad Gateway\r\n"
        };
        assert!(public.starts_with(expected_status));
        assert!(
            !public
                .windows(b"second-choice-text".len())
                .any(|bytes| bytes == b"second-choice-text")
        );
    }
}

#[tokio::test]
async fn a_stream_failure_reports_the_status_the_client_received() {
    for (error, error_kind, status) in [
        (
            AdapterError::InvalidRequest(crate::inference::InvalidRequest::request_limit(None)),
            "invalid_request",
            413,
        ),
        (
            AdapterError::InvalidRequest(crate::inference::InvalidRequest::invalid_field(Some(
                "top_k",
            ))),
            "invalid_request",
            400,
        ),
        (
            AdapterError::UnsupportedCapability,
            "unsupported_capability",
            400,
        ),
        (AdapterError::upstream("broken"), "upstream", 502),
    ] {
        let (mut writer, mut client) = connected_pair().await;
        let (sender, events) = mpsc::channel(1);
        sender.send(Err(error)).await.expect("stream receiver");
        drop(sender);

        let outcome = write_openai_stream(
            &mut writer,
            events,
            uncancelled(),
            "route".to_owned(),
            true,
            &mut |_| {},
        )
        .await
        .expect("stream write");
        drop(writer);
        let mut public = Vec::new();
        client
            .read_to_end(&mut public)
            .await
            .expect("public stream read");

        assert!(
            matches!(
                outcome,
                StreamWriteOutcome::Failed { error_kind: kind, status: recorded, committed: false, .. }
                    if kind == error_kind && recorded == status
            ),
            "{error_kind}: {outcome:?}"
        );
        assert!(public.starts_with(format!("HTTP/1.1 {status} ").as_bytes()));
    }
}

#[tokio::test]
async fn a_response_written_in_full_is_not_cancelled_by_the_client_closing_after_it() {
    let (mut writer, mut client) = connected_pair().await;
    let (disconnect, client_cancellation) = watch::channel(None);
    let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
    let cancellation = RequestCancellation::new(client_cancellation, route);

    let result = write_openai_response_cancellable(
        &mut writer,
        crate::router::openai::route_not_found(),
        cancellation.clone(),
    )
    .await;
    drop(writer);
    let mut public = Vec::new();
    client
        .read_to_end(&mut public)
        .await
        .expect("public response");
    drop(client);
    // The disconnect watcher signals once the client closes, before the router classifies.
    disconnect.send_replace(Some(RequestCancellationReason::ClientDisconnected));

    let span = tracing::info_span!("inference_request");
    assert!(public.starts_with(b"HTTP/1.1 404 Not Found\r\n"));
    assert_eq!(
        cancellation.reason(),
        Some(RequestCancellationReason::ClientDisconnected)
    );
    assert_eq!(
        record_written_response(&span, 404, None, &result, &cancellation),
        crate::router::metrics::RequestOutcome::Failed
    );
    assert_eq!(
        record_written_response(&span, 200, None, &result, &cancellation),
        crate::router::metrics::RequestOutcome::Success
    );
    assert_eq!(
        record_written_response(
            &span,
            504,
            Some(RequestCancellationReason::RequestTimedOut),
            &result,
            &cancellation
        ),
        crate::router::metrics::RequestOutcome::Cancelled
    );
}

#[tokio::test]
async fn a_response_that_cannot_be_written_fails_whatever_its_status() {
    let (mut writer, _client) = connected_pair().await;
    let response = crate::router::openai::OpenAiResponse {
        status: 200,
        content_type: "application/json".to_owned(),
        body: b"{}".to_vec(),
        passthrough_headers: vec![("invalid header".to_owned(), "value".to_owned())],
    };

    let ((result, outcome), _, output) = capture_records_async(|| async {
        let span = tracing::info_span!(
            "inference_request",
            status = tracing::field::Empty,
            outcome = tracing::field::Empty,
            error_kind = tracing::field::Empty,
        );
        let written = write_response(&mut writer, response, None, uncancelled(), &span).await;
        drop(span);
        written
    })
    .await;

    assert!(result.is_err());
    assert_eq!(outcome, crate::router::metrics::RequestOutcome::Failed);
    let close = output
        .lines()
        .find(|line| line.contains("close time.busy"))
        .expect("request span close");
    assert!(close.contains("status=200"), "{close}");
    assert!(close.contains("outcome=\"failed\""), "{close}");
    assert!(close.contains("error_kind=\"response_write\""), "{close}");

    // A failed write of a cancelled request stays a cancellation.
    let (_client, cancelled) = watch::channel(Some(RequestCancellationReason::ClientDisconnected));
    let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
    assert_eq!(
        record_written_response(
            &tracing::info_span!("inference_request"),
            200,
            None,
            &result.map(|()| ResponseWrite::Complete),
            &RequestCancellation::new(cancelled, route),
        ),
        crate::router::metrics::RequestOutcome::Cancelled
    );
}

#[tokio::test]
async fn a_response_cancelled_before_its_write_records_the_cancellation_answer() {
    let (mut writer, mut client) = connected_pair().await;
    let (disconnect, client_cancellation) = watch::channel(None);
    let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
    let cancellation = RequestCancellation::new(client_cancellation, route);
    disconnect.send_replace(Some(RequestCancellationReason::RequestTimedOut));

    // The router decided a successful answer, then the request timed out before its write.
    let result = write_openai_response_cancellable(
        &mut writer,
        crate::router::openai::models(Default::default()),
        cancellation.clone(),
    )
    .await;
    drop(writer);
    let mut public = Vec::new();
    client
        .read_to_end(&mut public)
        .await
        .expect("public response");
    let ((), _, output) = capture_records(|| {
        let span = tracing::info_span!(
            "inference_request",
            status = tracing::field::Empty,
            outcome = tracing::field::Empty,
            cancellation_reason = tracing::field::Empty,
        );
        assert_eq!(
            record_written_response(&span, 200, None, &result, &cancellation),
            crate::router::metrics::RequestOutcome::Cancelled
        );
    });

    assert_eq!(
        result.expect("written"),
        ResponseWrite::Replaced {
            reason: RequestCancellationReason::RequestTimedOut,
            status: 504,
        }
    );
    assert!(public.starts_with(b"HTTP/1.1 504 Gateway Timeout\r\n"));
    let close = output
        .lines()
        .find(|line| line.contains("close time.busy"))
        .expect("request span close");
    assert!(close.contains("status=504"), "{close}");
    assert!(close.contains("outcome=\"cancelled\""), "{close}");
}

#[tokio::test]
async fn a_complete_stream_succeeds_when_the_client_closes_right_after_it() {
    let (mut writer, mut client) = connected_pair().await;
    let (disconnect, client_cancellation) = watch::channel(None);
    let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
    let cancellation = RequestCancellation::new(client_cancellation, route);
    let (sender, events) = mpsc::channel(2);
    for event in [
        Event::TextDelta(ChatTextDelta {
            choice_index: 0,
            role: None,
            content: Some(chat_text_delta::Content::TextContent("hi".to_owned())),
        }),
        Event::Completed(Default::default()),
    ] {
        sender
            .send(Ok(ChatStreamEvent { event: Some(event) }))
            .await
            .expect("stream receiver");
    }
    drop(sender);

    let result = write_openai_stream(
        &mut writer,
        events,
        cancellation.clone(),
        "route".to_owned(),
        false,
        &mut |_| {},
    )
    .await;
    drop(writer);
    let mut public = Vec::new();
    client
        .read_to_end(&mut public)
        .await
        .expect("public stream");
    drop(client);
    disconnect.send_replace(Some(RequestCancellationReason::ClientDisconnected));

    assert!(public.ends_with(b"data: [DONE]\n\n"));
    assert_eq!(
        cancellation.reason(),
        Some(RequestCancellationReason::ClientDisconnected)
    );
    assert_eq!(
        record_stream(&tracing::info_span!("inference_request"), &result),
        crate::router::metrics::RequestOutcome::Success
    );
}

#[test]
fn fallback_exhaustion_is_temporary_unavailability_not_a_route_without_ready_targets() {
    let exhausted = fallback_exhausted_response(TargetSelectionError::Unavailable);
    let initial = selection_response(TargetSelectionError::Unavailable);

    assert_eq!(exhausted.status, 503);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&exhausted.body).expect("error JSON")["error"]
            ["code"],
        "model_unavailable"
    );
    assert_eq!(initial.status, 503);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&initial.body).expect("error JSON")["error"]["code"],
        "model_not_ready"
    );
}

#[test]
fn image_rejections_name_the_cause_and_carry_the_image_code() {
    let cause = crate::router::images::ImageValidationError::InvalidData(anyhow::Error::msg(
        "encoded payload is not canonical standard base64",
    ));
    let response = selection_response(TargetSelectionError::InvalidImage(cause));
    let body: serde_json::Value = serde_json::from_slice(&response.body).expect("error JSON");

    assert_eq!(response.status, 400);
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_image");
    assert_eq!(
        body["error"]["message"],
        "Invalid image data: encoded payload is not canonical standard base64"
    );

    // A target that cannot accept image input answers with its own safe message.
    let unsupported = crate::router::openai::image_unsupported_capability();
    let body: serde_json::Value = serde_json::from_slice(&unsupported.body).expect("error JSON");
    assert_eq!(unsupported.status, 400);
    assert_eq!(body["error"]["code"], "invalid_image");
    assert_eq!(
        body["error"]["message"],
        "Image input is not supported by the selected serving target."
    );
}

async fn connected_pair() -> (tokio::net::tcp::OwnedWriteHalf, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("public listener");
    let address = listener.local_addr().expect("public address");
    let client = tokio::net::TcpStream::connect(address)
        .await
        .expect("public client");
    let (server, _) = listener.accept().await.expect("public connection");
    let (_, writer) = server.into_split();
    (writer, client)
}

fn uncancelled() -> RequestCancellation {
    let (_client, client) = watch::channel::<Option<RequestCancellationReason>>(None);
    let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
    RequestCancellation::new(client, route)
}

#[tokio::test]
async fn a_streamed_request_records_its_time_to_first_token_and_duration() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("chat", true).await;
    let mut upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(2));
    upstream.start_stream("Hi").await;
    client.read_until("Hi").await;
    router.clock.advance(Duration::from_secs(3));
    upstream.finish_stream().await;
    client.read_to_end().await;
    router.finish(client).await;

    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                5,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                5,
                5.0
            ),
            distribution(REQUEST_TTFT, &[("route", &router.chat)], 5, 2.0),
        ]
    );
}

#[tokio::test]
async fn a_non_streamed_request_records_its_duration_without_time_to_first_token() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("chat", false).await;
    let mut upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(4));
    upstream.complete().await;
    client.read_to_end().await;
    router.finish(client).await;

    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                4,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                4,
                4.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_client_cancelling_mid_stream_records_a_cancelled_request_no_longer_in_flight() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("chat", true).await;
    let mut upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(1));
    upstream.start_stream("Hi").await;
    client.read_until("Hi").await;
    router.clock.advance(Duration::from_secs(2));
    assert_eq!(router.in_flight_sample(3).await, 1.0);
    drop(client.stream);
    client
        .handler
        .await
        .expect("connection task")
        .expect("connection");
    router.clock.advance(Duration::from_secs(1));

    assert_eq!(router.in_flight_sample(4).await, 0.0);
    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "cancelled")
                ],
                3,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", &router.chat),
                    ("request_kind", "chat"),
                    ("outcome", "cancelled")
                ],
                3,
                3.0
            ),
            distribution(REQUEST_TTFT, &[("route", &router.chat)], 3, 1.0),
        ]
    );
    let attempt = upstream_attempt(&router.chat);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&attempt, "outcome", "cancelled"), 3, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&attempt, "outcome", "cancelled"),
                3,
                3.0
            ),
            distribution(ATTEMPT_TTFT, &attempt, 3, 1.0),
        ]
    );
    drop(upstream);
}

#[tokio::test]
async fn a_fallback_counts_and_its_refused_attempt_is_part_of_time_to_first_token() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("fallback", true).await;

    router.node.invoker.invoked.notified().await;
    router.clock.advance(Duration::from_secs(2));
    router.release_refusal.notify_one();
    let mut upstream = router.next_upstream_call().await;
    router.clock.advance(Duration::from_secs(1));
    upstream.start_stream("Hi").await;
    client.read_until("Hi").await;
    router.clock.advance(Duration::from_secs(1));
    upstream.finish_stream().await;
    client.read_to_end().await;
    router.finish(client).await;

    let route = &router.fallback;
    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                4,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("outcome", "success")
                ],
                4,
                4.0
            ),
            distribution(REQUEST_TTFT, &[("route", route)], 4, 3.0),
            counter(
                REQUEST_FALLBACKS,
                &[("route", route), ("request_kind", "chat")],
                4,
                1.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_cancellation_a_node_reports_records_a_cancelled_request() {
    let router =
        MeteredRouter::start_with(Script::CancelledBy(RequestCancellationReason::RouteRevoked))
            .await;
    let mut client = router.send("fallback", false).await;

    client.read_to_end().await;
    router.finish(client).await;

    let route = &router.fallback;
    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("outcome", "cancelled")
                ],
                0,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("outcome", "cancelled")
                ],
                0,
                0.0
            ),
        ]
    );
    let local = local_attempt(route);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&local, "outcome", "cancelled"), 0, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&local, "outcome", "cancelled"),
                0,
                0.0
            ),
        ]
    );
}

#[tokio::test]
async fn admission_rejections_count_with_the_route_only_once_it_is_known() {
    let router = MeteredRouter::start().await;

    for route in ["missing", "unserved"] {
        let mut client = router.send(route, true).await;
        client.read_to_end().await;
        router.finish(client).await;
    }

    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[("request_kind", "chat"), ("outcome", "rejected")],
                0,
                1.0
            ),
            counter(
                REQUESTS,
                &[
                    ("route", &router.unserved),
                    ("request_kind", "chat"),
                    ("outcome", "rejected")
                ],
                0,
                1.0
            ),
        ]
    );
}

#[tokio::test]
async fn concurrent_requests_peak_in_flight_at_their_concurrency_then_return_to_zero() {
    let router = MeteredRouter::start().await;
    let mut clients = Vec::new();
    let mut upstreams = Vec::new();
    for _ in 0..3 {
        clients.push(router.send("chat", true).await);
        upstreams.push(router.next_upstream_call().await);
    }

    router.clock.advance(Duration::from_secs(1));
    assert_eq!(router.in_flight_sample(1).await, 3.0);
    for (upstream, client) in upstreams.iter_mut().zip(&mut clients) {
        upstream.start_stream("Hi").await;
        upstream.finish_stream().await;
        client.read_to_end().await;
    }
    for client in clients {
        router.finish(client).await;
    }
    router.clock.advance(Duration::from_secs(1));

    assert_eq!(router.in_flight_sample(2).await, 0.0);
    let samples = router.in_flight_values();
    assert_eq!(samples.iter().copied().fold(0.0, f64::max), 3.0);
}

#[tokio::test]
async fn a_streamed_attempt_records_its_tokens_and_decode_and_prefill_speeds() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("chat", true).await;
    let mut upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(2));
    upstream.start_stream("Hi").await;
    client.read_until("Hi").await;
    router.clock.advance(Duration::from_secs(3));
    upstream.delta(" there").await;
    client.read_until(" there").await;
    upstream
        .finish_stream_with_usage(Some(serde_json::json!({
            "prompt_tokens": 12, "completion_tokens": 7, "total_tokens": 19,
            "prompt_tokens_details": {"cached_tokens": 4},
        })))
        .await;
    client.read_to_end().await;
    router.finish(client).await;

    let attempt = upstream_attempt(&router.chat);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&attempt, "outcome", "success"), 5, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&attempt, "outcome", "success"),
                5,
                5.0
            ),
            distribution(ATTEMPT_TTFT, &attempt, 5, 2.0),
            counter(TOKENS_PROMPT, &attempt, 5, 12.0),
            counter(TOKENS_COMPLETION, &attempt, 5, 7.0),
            counter(TOKENS_CACHED_PROMPT, &attempt, 5, 4.0),
            // Six tokens after the first over the 3s between the first and last deltas.
            distribution(ATTEMPT_DECODE_RATE, &attempt, 5, 2.0),
            // Eight uncached prompt tokens over the 2s to the first token.
            distribution(ATTEMPT_PREFILL_RATE, &attempt, 5, 4.0),
        ]
    );
}

#[tokio::test]
async fn a_non_streamed_attempt_records_its_duration_and_tokens_without_speeds() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("chat", false).await;
    let mut upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(4));
    upstream.complete().await;
    client.read_to_end().await;
    router.finish(client).await;

    let attempt = upstream_attempt(&router.chat);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&attempt, "outcome", "success"), 4, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&attempt, "outcome", "success"),
                4,
                4.0
            ),
            counter(TOKENS_PROMPT, &attempt, 4, 21.0),
            counter(TOKENS_COMPLETION, &attempt, 4, 7.0),
        ]
    );
}

#[tokio::test]
async fn a_refused_attempt_counts_against_its_deployment_and_the_next_times_its_own_first_token() {
    let router = MeteredRouter::start().await;
    let mut client = router.send("fallback", true).await;

    router.node.invoker.invoked.notified().await;
    router.clock.advance(Duration::from_secs(2));
    router.release_refusal.notify_one();
    let mut upstream = router.next_upstream_call().await;
    router.clock.advance(Duration::from_secs(1));
    upstream.start_stream("Hi").await;
    client.read_until("Hi").await;
    router.clock.advance(Duration::from_secs(1));
    upstream.finish_stream().await;
    client.read_to_end().await;
    router.finish(client).await;

    let route = &router.fallback;
    let local = local_attempt(route);
    let upstream = upstream_attempt(route);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(
                ATTEMPTS,
                &with(&with(&local, "outcome", "failed"), "stage", "upstream"),
                2,
                1.0
            ),
            distribution(ATTEMPT_DURATION, &with(&local, "outcome", "failed"), 2, 2.0),
            counter(ATTEMPTS, &with(&upstream, "outcome", "success"), 4, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&upstream, "outcome", "success"),
                4,
                2.0
            ),
            distribution(ATTEMPT_TTFT, &upstream, 4, 1.0),
            counter(
                USAGE_MISSING,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("deployment", UPSTREAM_DEPLOYMENT)
                ],
                4,
                1.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_successful_attempt_without_usage_counts_missing_usage_and_no_tokens() {
    let router = MeteredRouter::start_with(Script::Complete).await;
    let mut client = router.send("fallback", false).await;

    client.read_to_end().await;
    router.finish(client).await;

    let route = &router.fallback;
    let local = local_attempt(route);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&local, "outcome", "success"), 0, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&local, "outcome", "success"),
                0,
                0.0
            ),
            counter(
                USAGE_MISSING,
                &[
                    ("route", route),
                    ("request_kind", "chat"),
                    ("deployment", DEPLOYMENT)
                ],
                0,
                1.0
            ),
        ]
    );
}

#[test]
fn a_failed_client_write_cancels_the_attempt_rather_than_failing_its_deployment() {
    let cases = [
        (
            Ok(StreamWriteOutcome::Complete),
            (RequestOutcome::Success, None),
        ),
        (
            Ok(StreamWriteOutcome::Cancelled(
                RequestCancellationReason::ClientDisconnected,
            )),
            (RequestOutcome::Cancelled, None),
        ),
        (
            Ok(StreamWriteOutcome::Failed {
                error_kind: "upstream",
                failure_stage: "sse_decoding",
                committed: true,
                status: 200,
            }),
            (RequestOutcome::Failed, Some("sse_decoding")),
        ),
        (
            Err(OpenAiWriteError::io(std::io::Error::from(
                std::io::ErrorKind::BrokenPipe,
            ))),
            (RequestOutcome::Cancelled, None),
        ),
        (
            Err(OpenAiWriteError::encoding(anyhow::anyhow!("frame"))),
            (RequestOutcome::Failed, Some("stream_encoding")),
        ),
    ];

    for (result, expected) in cases {
        assert_eq!(stream_attempt_end(&result), expected, "{result:?}");
    }
}

#[test]
fn a_target_refusing_a_request_mid_stream_rejects_the_attempt_as_without_a_stream() {
    for stage in ["invalid_request", "unsupported_capability"] {
        let result = Ok(StreamWriteOutcome::Failed {
            error_kind: stage,
            failure_stage: stage,
            committed: true,
            status: 200,
        });

        assert_eq!(
            stream_attempt_end(&result),
            (RequestOutcome::Rejected, Some(stage))
        );
    }
}

#[tokio::test]
async fn a_system_one_decision_answers_under_its_route_name_and_records_decision_samples() {
    let router = MeteredRouter::start_with(Script::Complete).await;

    let response = String::from_utf8(client_received(&router, SYSTEM_ONE_REQUEST).await)
        .expect("UTF-8 response");

    assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
    assert!(
        response.ends_with(
            r#"{"model":"decision","answers":{"urgent":{"type":"noul","noul":0.75}},"usage":{"input_tokens":4,"output_tokens":0}}"#
        ),
        "{response}"
    );
    assert_eq!(
        router.request_records(),
        vec![
            counter(
                REQUESTS,
                &[
                    ("route", &router.decision),
                    ("request_kind", "decision"),
                    ("outcome", "success")
                ],
                0,
                1.0
            ),
            distribution(
                REQUEST_DURATION,
                &[
                    ("route", &router.decision),
                    ("request_kind", "decision"),
                    ("outcome", "success")
                ],
                0,
                0.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_decision_engine_rejection_passes_through_with_its_status() {
    let router = MeteredRouter::start_with(Script::Passthrough).await;

    let response = String::from_utf8(client_received(&router, SYSTEM_ONE_REQUEST).await)
        .expect("UTF-8 response");

    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    assert!(
        response.ends_with(
            r#"{"error":{"code":null,"message":"The inference engine rejected the request.","param":null,"type":"upstream_error"}}"#
        ),
        "{response}"
    );
}

#[tokio::test]
async fn an_oversized_decision_is_a_context_length_error_that_never_falls_back() {
    let router = MeteredRouter::start_with(Script::InvalidRequest(
        crate::inference::InvalidRequest::context_length(),
    ))
    .await;
    let request = SYSTEM_ONE_REQUEST.replace(r#""model":"decision""#, r#""model":"fallback""#);

    let response =
        String::from_utf8(client_received(&router, &request).await).expect("UTF-8 response");

    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    assert!(
        response.ends_with(
            r#"{"error":{"code":"context_length_exceeded","message":"The request is larger than the deployment's context.","param":null,"type":"invalid_request_error"}}"#
        ),
        "{response}"
    );
    assert_eq!(router.node.invoker.invoked_models(), ["fallback"]);
    let local = local_attempt(&router.fallback)
        .into_iter()
        .map(|(name, value)| match name {
            "request_kind" => (name, "decision"),
            _ => (name, value),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(
                ATTEMPTS,
                &with(
                    &with(&local, "outcome", "rejected"),
                    "stage",
                    "invalid_request"
                ),
                0,
                1.0
            ),
            distribution(
                ATTEMPT_DURATION,
                &with(&local, "outcome", "rejected"),
                0,
                0.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_decision_on_a_route_serving_only_chat_is_invalid_and_never_dispatched() {
    let router = MeteredRouter::start_with(Script::Complete).await;
    let request = SYSTEM_ONE_REQUEST.replace(r#""model":"decision""#, r#""model":"chat""#);

    let response =
        String::from_utf8(client_received(&router, &request).await).expect("UTF-8 response");

    assert!(response.starts_with("HTTP/1.1 400 "), "{response}");
    assert!(
        response.ends_with(
            r#"{"error":{"code":"unsupported_request_kind","message":"The requested model route does not serve this kind of request.","param":"model","type":"invalid_request_error"}}"#
        ),
        "{response}"
    );
    assert!(router.node.invoker.invoked_models().is_empty());
}

#[test]
fn a_responses_answer_with_several_choices_fails_its_attempt_as_its_stream_would() {
    let answer = |choices: usize| {
        AdapterAttempt::Response(AdapterResponse::Complete(ChatCompletionResult {
            choices: vec![ChatCompletionChoice::default(); choices],
            ..Default::default()
        }))
    };

    assert_eq!(
        answered_attempt_end(OpenAiRoute::Chat(ChatRoute::Responses), None, &answer(2)),
        Some((RequestOutcome::Failed, Some("unexpected_choice")))
    );
    assert_eq!(
        answered_attempt_end(OpenAiRoute::Chat(ChatRoute::Responses), None, &answer(1)),
        Some((RequestOutcome::Success, None))
    );
    assert_eq!(
        answered_attempt_end(OpenAiRoute::Chat(ChatRoute::Completions), None, &answer(2)),
        Some((RequestOutcome::Success, None))
    );
}

#[tokio::test]
async fn a_request_dropped_mid_attempt_records_its_open_attempt_as_cancelled() {
    let router = MeteredRouter::start().await;
    let client = router.send("chat", true).await;
    let _upstream = router.next_upstream_call().await;

    router.clock.advance(Duration::from_secs(2));
    client.handler.abort();
    let ended = tokio::time::timeout(WAIT_DEADLINE, client.handler)
        .await
        .expect("connection deadline");

    assert!(ended.expect_err("aborted connection").is_cancelled());
    let attempt = upstream_attempt(&router.chat);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(ATTEMPTS, &with(&attempt, "outcome", "cancelled"), 2, 1.0),
            distribution(
                ATTEMPT_DURATION,
                &with(&attempt, "outcome", "cancelled"),
                2,
                2.0
            ),
        ]
    );
}

#[tokio::test]
async fn a_stream_failing_midway_fails_its_attempt_and_keeps_the_usage_that_arrived() {
    let router = MeteredRouter::start_with(Script::StreamThenFail(vec![
        Event::Started(ChatStreamStarted {
            completion_id: "completion-a".to_owned(),
            model_id: "fallback".to_owned(),
            created_at_unix_seconds: 7,
        }),
        Event::TextDelta(ChatTextDelta {
            choice_index: 0,
            role: None,
            content: Some(chat_text_delta::Content::TextContent("Hi".to_owned())),
        }),
        Event::Usage(TokenUsage {
            prompt_tokens: 9,
            completion_tokens: 3,
            total_tokens: 12,
            cached_prompt_tokens: Some(2),
            reasoning_tokens: None,
        }),
    ]))
    .await;
    let mut client = router.send("fallback", true).await;

    client.read_to_end().await;
    router.finish(client).await;

    let local = local_attempt(&router.fallback);
    assert_eq!(
        router.attempt_records(),
        vec![
            counter(
                ATTEMPTS,
                &with(&with(&local, "outcome", "failed"), "stage", "upstream"),
                0,
                1.0
            ),
            distribution(ATTEMPT_DURATION, &with(&local, "outcome", "failed"), 0, 0.0),
            distribution(ATTEMPT_TTFT, &local, 0, 0.0),
            counter(TOKENS_PROMPT, &local, 0, 9.0),
            counter(TOKENS_COMPLETION, &local, 0, 3.0),
            counter(TOKENS_CACHED_PROMPT, &local, 0, 2.0),
        ]
    );
}

/// A router metering into a capturing recorder on a clock that moves only when advanced, with
/// routes `chat` (one upstream target), `fallback` (an in-process target that refuses when
/// released, then the upstream target) and `unserved` (a deployment without a target).
struct MeteredRouter {
    router: crate::router::InferenceRouter,
    clock: FrozenClock,
    recorder: Arc<CapturingRecorder>,
    node: crate::router::peer_service::test_support::Fixture,
    release_refusal: Arc<tokio::sync::Notify>,
    upstream_calls: tokio::sync::Mutex<mpsc::Receiver<tokio::net::TcpStream>>,
    router_listener: tokio::net::TcpListener,
    chat: String,
    fallback: String,
    unserved: String,
    decision: String,
    _site: tempfile::TempDir,
}

impl MeteredRouter {
    async fn start() -> Self {
        let release = Arc::new(tokio::sync::Notify::new());
        Self::start_scripted(Script::RefuseWhenReleased(Arc::clone(&release)), release).await
    }

    /// Starts with the in-process target playing `script` instead of the released refusal.
    async fn start_with(script: Script) -> Self {
        Self::start_scripted(script, Arc::new(tokio::sync::Notify::new())).await
    }

    async fn start_scripted(script: Script, release_refusal: Arc<tokio::sync::Notify>) -> Self {
        use dotcore_proto::dotcore::inference::v1::{
            CreateRouteRequest, FallbackGroup, FallbackGroupSelection,
        };

        use crate::{
            model::{
                controller::serving_targets::{
                    NodeDispatch, NodeServingTarget, NodeTransport, ServingTarget,
                    ServingTargetsPublisher, upstream_target_for_test_with_dispatch,
                },
                domain::ModelDeploymentId,
            },
            router::{
                adapters::peer::PeerInferenceClient,
                peer_service::test_support::{Fixture, GENERATION},
            },
        };

        let site = crate::test_support::canonical_tempdir().expect("site directory");
        let path = site.path().join("site.yaml");
        let mut declaration = crate::test_support::routed_site(&[
            DEPLOYMENT,
            UPSTREAM_DEPLOYMENT,
            "unserved-deployment",
        ]);
        // The in-process deployment serves both request kinds.
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            local,
        )) = declaration.model_deployments[0]
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        else {
            panic!("local fixture snapshot");
        };
        local.request_kinds = BOTH_KINDS.iter().map(|kind| *kind as i32).collect();
        std::fs::write(
            &path,
            dotcore_proto::yaml::to_string(&declaration).expect("site YAML"),
        )
        .expect("site fixture");
        let site_repository = dotcore_site::repository::SiteRepository::load(
            &path,
            crate::test_support::site_declaration_rules(),
        )
        .expect("site");
        let (publisher, targets) = ServingTargetsPublisher::new();
        let routes = crate::route_manager::RouteManager::new(
            crate::route_manager::RouteDeclarationRepository::new(site_repository),
            crate::route_manager::RouteAvailabilityTargetsSource::new(targets.clone()),
        )
        .expect("route manager");
        let create = |name: &str, groups: &[&str]| {
            routes
                .create(CreateRouteRequest {
                    name: name.to_owned(),
                    enabled: true,
                    fallback_groups: groups
                        .iter()
                        .map(|deployment| FallbackGroup {
                            selection: FallbackGroupSelection::Single as i32,
                            model_deployment_ids: vec![(*deployment).to_owned()],
                        })
                        .collect(),
                })
                .expect("created route")
                .route
                .expect("route declaration")
                .inference_route_id
        };
        let chat = create("chat", &[UPSTREAM_DEPLOYMENT]);
        let fallback = create("fallback", &[DEPLOYMENT, UPSTREAM_DEPLOYMENT]);
        let unserved = create("unserved", &["unserved-deployment"]);
        let decision = create("decision", &[DEPLOYMENT]);

        let node = Fixture::serving(script);
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let upstream_port = upstream_listener
            .local_addr()
            .expect("upstream address")
            .port();
        publisher.publish(vec![
            ServingTarget::Node(NodeServingTarget::new(
                ModelDeploymentId(DEPLOYMENT.to_owned()),
                GENERATION,
                WORKER_NODE_ID.to_owned(),
                dotcore_catalog::CatalogModelId(LOCAL_MODEL.to_owned()),
                None,
                BOTH_KINDS.into(),
                NodeDispatch {
                    transport: NodeTransport::InProcess,
                    manager_session: Fixture::manager_session(),
                    node_incarnation: Fixture::node_incarnation(),
                    binding_token: node.binding_token.clone(),
                },
            )),
            ServingTarget::Upstream(upstream_target_for_test_with_dispatch(
                UPSTREAM_DEPLOYMENT,
                &format!("http://127.0.0.1:{upstream_port}/v1"),
                "private-model",
                "test-secret",
            )),
        ]);
        let (calls, upstream_calls) = mpsc::channel(8);
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = upstream_listener.accept().await.expect("upstream call");
                crate::test_support::http::read_request(&mut stream).await;
                if calls.send(stream).await.is_err() {
                    return;
                }
            }
        });

        let clock = FrozenClock::new();
        let recorder = Arc::new(CapturingRecorder::default());
        let router = crate::router::InferenceRouter::new(targets)
            .with_routes(routes)
            .with_peer_client(PeerInferenceClient::new(None, node.admission()))
            .with_metrics(recorder.clone(), Arc::new(clock.clone()));
        Self {
            router,
            clock,
            recorder,
            node,
            release_refusal,
            upstream_calls: tokio::sync::Mutex::new(upstream_calls),
            router_listener: tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("router listener"),
            chat,
            fallback,
            unserved,
            decision,
            _site: site,
        }
    }

    /// Sends one chat request for `route` through the router's connection handling.
    async fn send(&self, route: &str, stream: bool) -> RouterClient {
        let body = format!(
            r#"{{"model":"{route}","messages":[{{"role":"user","content":"Hello"}}],"stream":{stream}}}"#
        );
        self.send_to("/v1/chat/completions", &body).await
    }

    /// Sends one request `body` to `path` through the router's connection handling.
    async fn send_to(&self, path: &str, body: &str) -> RouterClient {
        let address = self.router_listener.local_addr().expect("router address");
        let mut client = tokio::net::TcpStream::connect(address)
            .await
            .expect("router client");
        let (server, _) = self
            .router_listener
            .accept()
            .await
            .expect("router connection");
        let router = self.router.clone();
        let handler = tokio::spawn(async move {
            let permit = Arc::new(tokio::sync::Semaphore::new(1))
                .acquire_owned()
                .await
                .expect("request permit");
            super::handle_inference_connection(
                server,
                router,
                permit,
                tokio::time::Instant::now() + Duration::from_secs(3600),
            )
            .await
        });
        client
            .write_all(
                format!(
                    "POST {path} HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .expect("router request");
        RouterClient {
            stream: client,
            received: Vec::new(),
            handler,
        }
    }

    async fn next_upstream_call(&self) -> UpstreamCall {
        let stream = tokio::time::timeout(WAIT_DEADLINE, self.upstream_calls.lock().await.recv())
            .await
            .expect("upstream call deadline")
            .expect("upstream call");
        UpstreamCall(stream)
    }

    /// Waits for the client's connection handling to end, after which its request is recorded.
    async fn finish(&self, client: RouterClient) {
        drop(client.stream);
        tokio::time::timeout(WAIT_DEADLINE, client.handler)
            .await
            .expect("connection deadline")
            .expect("connection task")
            .expect("connection");
    }

    /// Request records other than in-flight samples, in recording order.
    fn request_records(&self) -> Vec<MetricRecord> {
        self.recorder
            .records()
            .into_iter()
            .filter(|record| is_request_record(record) && record.metric != REQUEST_IN_FLIGHT)
            .collect()
    }

    /// Attempt, token, and usage records, in recording order.
    fn attempt_records(&self) -> Vec<MetricRecord> {
        self.recorder
            .records()
            .into_iter()
            .filter(|record| !is_request_record(record))
            .collect()
    }

    /// The `chat` route's chat in-flight sample observed `seconds` after the clock started, waiting
    /// for the sampler to take it.
    async fn in_flight_sample(&self, seconds: u64) -> f64 {
        let observed_at = FrozenClock::wall_start() + Duration::from_secs(seconds);
        let labels = LabelSet::new()
            .with("route", self.chat.clone())
            .with("request_kind", "chat");
        let find = |records: &[MetricRecord]| {
            records.iter().find_map(|record| match record.value {
                RecordValue::Gauge(value)
                    if record.metric == REQUEST_IN_FLIGHT
                        && record.labels == labels
                        && record.observed_at == observed_at =>
                {
                    Some(value)
                }
                _ => None,
            })
        };
        tokio::time::timeout(WAIT_DEADLINE, async {
            let mut changes = self.recorder.changes.subscribe();
            loop {
                if let Some(value) = find(&self.recorder.records()) {
                    return value;
                }
                changes.changed().await.expect("recorder");
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "no in-flight sample at {seconds}s: {:?}",
                self.recorder.records()
            )
        })
    }

    /// Every chat in-flight sample of the `chat` route.
    fn in_flight_values(&self) -> Vec<f64> {
        let labels = LabelSet::new()
            .with("route", self.chat.clone())
            .with("request_kind", "chat");
        self.recorder
            .records()
            .into_iter()
            .filter_map(|record| match record.value {
                RecordValue::Gauge(value)
                    if record.metric == REQUEST_IN_FLIGHT && record.labels == labels =>
                {
                    Some(value)
                }
                _ => None,
            })
            .collect()
    }
}

const UPSTREAM_DEPLOYMENT: &str = "upstream-deployment";

/// Catalog model of the in-process target's deployment.
const LOCAL_MODEL: &str = "model-a";

/// Request kinds the in-process target's deployment serves.
const BOTH_KINDS: [InferenceRequestKind; 2] =
    [InferenceRequestKind::Chat, InferenceRequestKind::Decision];

/// System One request for the `decision` route, answered by the scripted decision result.
const SYSTEM_ONE_REQUEST: &str = r#"{"model":"decision","state":"Refund please","questions":{"urgent":{"type":"noul","instructions":"Is it urgent?"}}}"#;

/// Bounds every wait on the router, the fake upstream, or the sampler.
const WAIT_DEADLINE: Duration = Duration::from_secs(10);

/// The client end of one routed request.
struct RouterClient {
    stream: tokio::net::TcpStream,
    received: Vec<u8>,
    handler: tokio::task::JoinHandle<Result<(), super::ConnectionError>>,
}

impl RouterClient {
    async fn read_until(&mut self, text: &str) {
        tokio::time::timeout(WAIT_DEADLINE, async {
            while !String::from_utf8_lossy(&self.received).contains(text) {
                let mut buffer = [0; 1024];
                let read = self
                    .stream
                    .read(&mut buffer)
                    .await
                    .expect("router response");
                assert!(read > 0, "response ended before {text:?}");
                self.received.extend_from_slice(&buffer[..read]);
            }
        })
        .await
        .expect("router response deadline");
    }

    async fn read_to_end(&mut self) {
        tokio::time::timeout(WAIT_DEADLINE, self.stream.read_to_end(&mut self.received))
            .await
            .expect("router response deadline")
            .expect("router response");
    }
}

/// One request the fake upstream received, answered as the test drives it.
struct UpstreamCall(tokio::net::TcpStream);

impl UpstreamCall {
    async fn start_stream(&mut self, text: &str) {
        let chunk = serde_json::json!({
            "id": "chatcmpl_metered", "object": "chat.completion.chunk", "created": 1,
            "model": "private-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": null}],
        });
        self.write(&format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\n"
        ))
        .await;
    }

    async fn delta(&mut self, text: &str) {
        let chunk = serde_json::json!({
            "id": "chatcmpl_metered", "object": "chat.completion.chunk", "created": 1,
            "model": "private-model",
            "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}],
        });
        self.write(&format!("data: {chunk}\n\n")).await;
    }

    async fn finish_stream(&mut self) {
        self.finish_stream_with_usage(None).await;
    }

    /// Ends the stream, with a terminal usage chunk when `usage` is given.
    async fn finish_stream_with_usage(&mut self, usage: Option<serde_json::Value>) {
        let chunk = serde_json::json!({
            "id": "chatcmpl_metered", "object": "chat.completion.chunk", "created": 1,
            "model": "private-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
        });
        self.write(&format!("data: {chunk}\n\n")).await;
        if let Some(usage) = usage {
            let chunk = serde_json::json!({
                "id": "chatcmpl_metered", "object": "chat.completion.chunk", "created": 1,
                "model": "private-model", "choices": [], "usage": usage,
            });
            self.write(&format!("data: {chunk}\n\n")).await;
        }
        self.write("data: [DONE]\n\n").await;
        self.0.shutdown().await.expect("upstream close");
    }

    async fn complete(&mut self) {
        let body = crate::router::test_fixtures::completion().to_string();
        self.write(&format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
    }

    async fn write(&mut self, bytes: &str) {
        self.0
            .write_all(bytes.as_bytes())
            .await
            .expect("upstream response");
    }
}

/// Metric recorder keeping every record, announcing each batch.
#[derive(Default)]
struct CapturingRecorder {
    records: std::sync::Mutex<Vec<MetricRecord>>,
    changes: watch::Sender<usize>,
}

impl CapturingRecorder {
    fn records(&self) -> Vec<MetricRecord> {
        self.records.lock().expect("records").clone()
    }
}

impl MetricRecorder for CapturingRecorder {
    fn record(&self, records: Vec<MetricRecord>) {
        self.records.lock().expect("records").extend(records);
        self.changes.send_modify(|count| *count += 1);
    }
}

/// Clock that stands still until advanced, so measured durations equal the advances exactly.
#[derive(Clone)]
struct FrozenClock {
    started: tokio::time::Instant,
    offset: Arc<watch::Sender<Duration>>,
}

impl FrozenClock {
    fn new() -> Self {
        Self {
            started: tokio::time::Instant::now(),
            offset: Arc::new(watch::Sender::new(Duration::ZERO)),
        }
    }

    /// Wall time when the clock starts.
    fn wall_start() -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    fn advance(&self, by: Duration) {
        self.offset.send_modify(|offset| *offset += by);
    }
}

impl dotcore_domain::clock::Clock for FrozenClock {
    fn now(&self) -> tokio::time::Instant {
        self.started + *self.offset.borrow()
    }

    fn wall_now(&self) -> std::time::SystemTime {
        Self::wall_start() + *self.offset.borrow()
    }

    fn sleep_until(&self, deadline: tokio::time::Instant) -> dotcore_domain::clock::ClockSleep {
        let started = self.started;
        let mut offset = self.offset.subscribe();
        Box::pin(async move {
            while started + *offset.borrow_and_update() < deadline {
                if offset.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

fn counter(metric: &str, labels: &[(&str, &str)], seconds: u64, delta: f64) -> MetricRecord {
    MetricRecord::counter(
        metric,
        labels.iter().copied().collect(),
        FrozenClock::wall_start() + Duration::from_secs(seconds),
        delta,
    )
}

fn distribution(metric: &str, labels: &[(&str, &str)], seconds: u64, value: f64) -> MetricRecord {
    MetricRecord::distribution(
        metric,
        labels.iter().copied().collect(),
        FrozenClock::wall_start() + Duration::from_secs(seconds),
        value,
    )
}

/// `labels` with one more label.
fn with<'a>(
    labels: &[(&'static str, &'a str)],
    name: &'static str,
    value: &'a str,
) -> Vec<(&'static str, &'a str)> {
    let mut labels = labels.to_vec();
    labels.push((name, value));
    labels
}

/// Labels of an attempt of `route` on the upstream target, which has no node or model.
fn upstream_attempt(route: &str) -> Vec<(&'static str, &str)> {
    vec![
        ("route", route),
        ("request_kind", "chat"),
        ("deployment", UPSTREAM_DEPLOYMENT),
        ("target_kind", "upstream"),
    ]
}

/// Labels of an attempt of `route` on the in-process target.
fn local_attempt(route: &str) -> Vec<(&'static str, &str)> {
    vec![
        ("route", route),
        ("request_kind", "chat"),
        ("deployment", DEPLOYMENT),
        ("model", LOCAL_MODEL),
        ("node", WORKER_NODE_ID),
        ("target_kind", "local"),
    ]
}

/// Whether `record` measures a whole request rather than one of its attempts.
fn is_request_record(record: &MetricRecord) -> bool {
    record.metric == REQUESTS || record.metric.starts_with("router.request.")
}

/// Sends `body` to the System One route and returns every byte the client received.
async fn client_received(router: &MeteredRouter, body: &str) -> Vec<u8> {
    let mut client = router.send_to("/v1/systemone", body).await;
    client.read_to_end().await;
    let received = std::mem::take(&mut client.received);
    router.finish(client).await;
    received
}
