//! OpenAI chat and Responses API SSE stream framing.

use dotcore_proto::dotcore::inference::v1::ChatStreamEvent;

use super::{
    errors::OpenAiWriteError,
    response::{
        CancellableWrite, ResponseWrite, write_bytes_cancellable, write_cancelled_before_headers,
        write_openai_response_cancellable,
    },
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamWriteOutcome {
    Complete,

    Cancelled(crate::router::cancellation::RequestCancellationReason),

    /// The stream failed with a safe kind and stage, before or after its response was committed;
    /// `status` is the HTTP status the client received.
    Failed {
        error_kind: &'static str,
        failure_stage: &'static str,
        committed: bool,
        status: u16,
    },
}

pub(super) async fn write_openai_stream(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    mut events: EventReceiver,
    mut cancellation: crate::router::cancellation::RequestCancellation,
    admitted_route_name: String,
    include_usage: bool,
    observe: &mut (dyn FnMut(&ChatStreamEvent) + Send),
) -> Result<StreamWriteOutcome, OpenAiWriteError> {
    let mut committed = false;
    let mut encoder = crate::router::openai::StreamEncoder::new(admitted_route_name, include_usage);
    loop {
        let event = tokio::select! { biased; reason = cancellation.cancelled() => return Ok(StreamWriteOutcome::Cancelled(reason)), event = events.recv() => event };
        let Some(event) = event else {
            return if committed {
                write_stream_bytes(writer, b"data: [DONE]\n\n", &mut cancellation).await
            } else {
                Ok(StreamWriteOutcome::Complete)
            };
        };
        let event = match event {
            Ok(event) => event,
            Err(crate::router::adapters::AdapterError::Cancelled(reason)) => {
                return Ok(StreamWriteOutcome::Cancelled(reason));
            }
            Err(error) => {
                return write_stream_failure(
                    writer,
                    StreamFailure::from_error(&error, crate::router::openai::stream_error_frame()),
                    &mut cancellation,
                    committed,
                )
                .await;
            }
        };
        observe(&event);
        let Some(frame) = encoder.encode(event) else {
            continue;
        };
        if !committed {
            if let Some(outcome) = write_stream_start(writer, &frame, &mut cancellation).await? {
                return Ok(outcome);
            }
            committed = true;
        } else if !matches!(
            write_bytes_cancellable(writer, &frame, &mut cancellation).await?,
            CancellableWrite::Complete
        ) {
            return Ok(cancelled(&cancellation));
        }
    }
}

pub(super) async fn write_responses_stream(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    mut events: EventReceiver,
    mut cancellation: crate::router::cancellation::RequestCancellation,
    admitted_route_name: String,
    observe: &mut (dyn FnMut(&ChatStreamEvent) + Send),
) -> Result<StreamWriteOutcome, OpenAiWriteError> {
    let mut committed = false;
    let mut encoder = crate::router::openai::responses::StreamEncoder::new(admitted_route_name);
    loop {
        let event = tokio::select! { biased; reason = cancellation.cancelled() => return Ok(StreamWriteOutcome::Cancelled(reason)), event = events.recv() => event };
        let Some(event) = event else {
            return Ok(StreamWriteOutcome::Complete);
        };
        let failure_frame = crate::router::openai::responses::stream_error_frame;
        let frames = match event.map(|event| {
            observe(&event);
            encoder.encode(event)
        }) {
            Ok(Ok(frames)) => frames,
            Ok(Err(_)) => {
                return write_stream_failure(
                    writer,
                    StreamFailure::invalid_engine_response("unexpected_choice", failure_frame()),
                    &mut cancellation,
                    committed,
                )
                .await;
            }
            Err(crate::router::adapters::AdapterError::Cancelled(reason)) => {
                return Ok(StreamWriteOutcome::Cancelled(reason));
            }
            Err(error) => {
                return write_stream_failure(
                    writer,
                    StreamFailure::from_error(&error, failure_frame()),
                    &mut cancellation,
                    committed,
                )
                .await;
            }
        };
        for frame in frames {
            if !committed {
                if let Some(outcome) = write_stream_start(writer, &frame, &mut cancellation).await?
                {
                    return Ok(outcome);
                }
                committed = true;
            } else if !matches!(
                write_bytes_cancellable(writer, &frame, &mut cancellation).await?,
                CancellableWrite::Complete
            ) {
                return Ok(cancelled(&cancellation));
            }
        }
    }
}

type EventReceiver =
    tokio::sync::mpsc::Receiver<Result<ChatStreamEvent, crate::router::adapters::AdapterError>>;

/// A terminal stream failure: its public answer before commit and its SSE frame after commit.
struct StreamFailure {
    error_kind: &'static str,
    failure_stage: &'static str,
    response: crate::router::openai::OpenAiResponse,
    frame: Vec<u8>,
}

impl StreamFailure {
    fn from_error(error: &crate::router::adapters::AdapterError, frame: Vec<u8>) -> Self {
        let failure_stage = crate::router::adapters::failure_stage(error);
        match error {
            crate::router::adapters::AdapterError::InvalidRequest(invalid) => Self {
                error_kind: "invalid_request",
                failure_stage,
                response: crate::router::openai::request_error(*invalid),
                frame,
            },
            crate::router::adapters::AdapterError::UnsupportedCapability => Self {
                error_kind: "unsupported_capability",
                failure_stage,
                response: crate::router::openai::image_unsupported_capability(),
                frame,
            },
            _ => Self::invalid_engine_response(failure_stage, frame),
        }
    }

    fn invalid_engine_response(failure_stage: &'static str, frame: Vec<u8>) -> Self {
        Self {
            error_kind: "upstream",
            failure_stage,
            response: engine_failure_response(failure_stage),
            frame,
        }
    }
}

/// Projects an engine failure before commit, telling an unreachable engine apart from an
/// invalid engine response without exposing the engine's details.
pub(super) fn engine_failure_response(
    failure_stage: &str,
) -> crate::router::openai::OpenAiResponse {
    match failure_stage {
        "connect" => crate::router::openai::engine_unreachable(),
        _ => crate::router::openai::adapter_error(),
    }
}

async fn write_stream_failure(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    failure: StreamFailure,
    cancellation: &mut crate::router::cancellation::RequestCancellation,
    committed: bool,
) -> Result<StreamWriteOutcome, OpenAiWriteError> {
    let outcome = StreamWriteOutcome::Failed {
        error_kind: failure.error_kind,
        failure_stage: failure.failure_stage,
        committed,
        // A committed stream already answered 200; otherwise the failure's own answer is sent.
        status: if committed {
            200
        } else {
            failure.response.status
        },
    };
    if committed {
        return match write_bytes_cancellable(writer, &failure.frame, cancellation).await? {
            CancellableWrite::Complete => Ok(outcome),
            CancellableWrite::CancelledBeforeWrite(_) | CancellableWrite::CancelledAfterWrite => {
                Ok(cancelled(cancellation))
            }
        };
    }
    Ok(
        match write_openai_response_cancellable(writer, failure.response, cancellation.clone())
            .await?
        {
            ResponseWrite::Complete => outcome,
            ResponseWrite::Replaced { reason, .. } | ResponseWrite::Interrupted(reason) => {
                StreamWriteOutcome::Cancelled(reason)
            }
        },
    )
}

async fn write_stream_start(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    frame: &[u8],
    cancellation: &mut crate::router::cancellation::RequestCancellation,
) -> Result<Option<StreamWriteOutcome>, OpenAiWriteError> {
    let mut bytes =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n".to_vec();
    bytes.extend_from_slice(frame);
    match write_bytes_cancellable(writer, &bytes, cancellation).await? {
        CancellableWrite::Complete => Ok(None),
        CancellableWrite::CancelledBeforeWrite(reason) => {
            write_cancelled_before_headers(writer, reason).await?;
            Ok(Some(StreamWriteOutcome::Cancelled(reason)))
        }
        CancellableWrite::CancelledAfterWrite => Ok(Some(cancelled(cancellation))),
    }
}

async fn write_stream_bytes(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    bytes: &[u8],
    cancellation: &mut crate::router::cancellation::RequestCancellation,
) -> Result<StreamWriteOutcome, OpenAiWriteError> {
    match write_bytes_cancellable(writer, bytes, cancellation).await? {
        CancellableWrite::Complete => Ok(StreamWriteOutcome::Complete),
        CancellableWrite::CancelledBeforeWrite(_) | CancellableWrite::CancelledAfterWrite => {
            Ok(cancelled(cancellation))
        }
    }
}

fn cancelled(
    cancellation: &crate::router::cancellation::RequestCancellation,
) -> StreamWriteOutcome {
    StreamWriteOutcome::Cancelled(
        cancellation
            .reason()
            .unwrap_or(crate::router::cancellation::RequestCancellationReason::ClientDisconnected),
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::{mpsc, watch},
        time::{Duration, timeout},
    };

    use super::*;
    use crate::{
        model::controller::serving_targets::ServingTargetsRevocationReason,
        router::{
            adapters::AdapterError,
            cancellation::{RequestCancellation, RequestCancellationReason},
        },
    };

    #[tokio::test]
    async fn chat_stream_failure_before_start_emits_a_safe_openai_error_response() {
        let (mut client, mut writer) = connected_writer().await;
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Err(AdapterError::upstream(
                "private endpoint https://provider.example/v1 model private-model key secret",
            )))
            .await
            .expect("stream receiver");
        drop(sender);

        let result = write_openai_stream(
            &mut writer,
            receiver,
            uncancelled(),
            "fixture-route".to_owned(),
            true,
            &mut |_| {},
        )
        .await;
        writer.shutdown().await.expect("stream writer shutdown");
        let body = read_stream(&mut client).await;

        assert_eq!(
            result.expect("stream write"),
            StreamWriteOutcome::Failed {
                error_kind: "upstream",
                failure_stage: "upstream",
                committed: false,
                status: 502,
            }
        );
        assert_eq!(
            response_body(&body),
            json!({"error":{"message":"The inference engine returned an invalid response.","type":"upstream_error","param":null,"code":null}})
        );
        assert!(!body.contains("provider.example"));
        assert!(!body.contains("private-model"));
        assert!(!body.contains("secret"));
        assert!(!body.contains("[DONE]"));
    }

    #[tokio::test]
    async fn responses_stream_failure_before_start_emits_a_safe_openai_error_response() {
        let (mut client, mut writer) = connected_writer().await;
        let (sender, receiver) = mpsc::channel(1);
        sender
            .send(Err(AdapterError::upstream(
                "private endpoint https://provider.example/v1 model private-model key secret",
            )))
            .await
            .expect("stream receiver");
        drop(sender);

        let result = write_responses_stream(
            &mut writer,
            receiver,
            uncancelled(),
            "fixture-route".to_owned(),
            &mut |_| {},
        )
        .await;
        writer.shutdown().await.expect("stream writer shutdown");
        let body = read_stream(&mut client).await;

        assert_eq!(
            result.expect("stream write"),
            StreamWriteOutcome::Failed {
                error_kind: "upstream",
                failure_stage: "upstream",
                committed: false,
                status: 502,
            }
        );
        assert_eq!(
            response_body(&body),
            json!({"error":{"message":"The inference engine returned an invalid response.","type":"upstream_error","param":null,"code":null}})
        );
        assert!(!body.contains("provider.example"));
        assert!(!body.contains("private-model"));
        assert!(!body.contains("secret"));
    }

    #[tokio::test]
    async fn a_failure_answer_cancelled_before_its_write_reports_the_cancellation() {
        let (mut client, mut writer) = connected_writer().await;
        let (_client, client_cancellation) =
            watch::channel(Some(RequestCancellationReason::RequestTimedOut));
        let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
        let mut cancellation = RequestCancellation::new(client_cancellation, route);

        let outcome = write_stream_failure(
            &mut writer,
            StreamFailure::from_error(
                &AdapterError::upstream("broken"),
                crate::router::openai::stream_error_frame(),
            ),
            &mut cancellation,
            false,
        )
        .await;
        writer.shutdown().await.expect("stream writer shutdown");
        let body = read_stream(&mut client).await;

        assert_eq!(
            outcome.expect("stream write"),
            StreamWriteOutcome::Cancelled(RequestCancellationReason::RequestTimedOut)
        );
        assert!(body.starts_with("HTTP/1.1 504 Gateway Timeout\r\n"));
    }

    async fn connected_writer() -> (tokio::net::TcpStream, tokio::net::tcp::OwnedWriteHalf) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
        let address = listener.local_addr().expect("address");
        let client = tokio::net::TcpStream::connect(address)
            .await
            .expect("client");
        let (server, _) = listener.accept().await.expect("server");
        let (_, writer) = server.into_split();
        (client, writer)
    }

    async fn read_stream(client: &mut tokio::net::TcpStream) -> String {
        let mut bytes = Vec::new();
        timeout(Duration::from_secs(1), client.read_to_end(&mut bytes))
            .await
            .expect("stream body read timed out")
            .expect("stream body");
        String::from_utf8(bytes).expect("UTF-8 stream")
    }

    fn response_body(body: &str) -> Value {
        let (_, body) = body.split_once("\r\n\r\n").expect("HTTP response body");
        serde_json::from_str(body).expect("OpenAI response JSON")
    }

    fn uncancelled() -> RequestCancellation {
        let (_client, client) = watch::channel(None);
        let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
        RequestCancellation::new(client, route)
    }
}
