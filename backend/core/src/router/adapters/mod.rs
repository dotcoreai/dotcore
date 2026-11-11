//! Private inference-engine adapter dispatch for selected serving targets.

pub(crate) mod openai_compatible;
pub(crate) mod peer;

use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionResult, ChatStreamEvent, DecisionResult,
};
use tokio::sync::mpsc;
use tracing::{Instrument, Span, debug_span, error, field, warn};

#[cfg(test)]
use super::openai_compatible_codec;
use super::{
    cancellation::{RequestCancellation, RequestCancellationReason},
    selection::SelectedInferenceTarget,
};
use crate::{
    inference::{
        DecisionResponse, InferenceAttempt, InferenceAttemptDisposition, InferenceError,
        InferencePassthroughResponse, InferenceRequest, InferenceResponse, InvalidRequest,
    },
    model::controller::serving_targets::LocalServingTarget,
};

/// Result from one private serving adapter before public OpenAI API encoding.
pub(crate) enum AdapterResponse {
    Complete(ChatCompletionResult),

    Stream(mpsc::Receiver<Result<ChatStreamEvent, AdapterError>>),

    Decision(DecisionResult),

    Upstream(UpstreamResponse),
}

/// Whether an unsuccessful adapter attempt may advance to the next fallback group.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdapterAttemptDisposition {
    SafeToFallback,

    StartedOrAmbiguous,

    /// The request itself is invalid, so no target may be tried; no engine work is implied.
    NonRetryable,
}

/// One adapter attempt, retaining whether its failure can safely advance fallback.
pub(crate) enum AdapterAttempt {
    Response(AdapterResponse),

    Failed {
        disposition: AdapterAttemptDisposition,
        error: AdapterError,
    },
}

impl AdapterAttempt {
    pub(crate) fn from_result(result: Result<AdapterResponse, AdapterError>) -> Self {
        match result {
            Ok(response) => Self::Response(response),
            Err(error) => Self::Failed {
                disposition: failure_disposition(&error),
                error,
            },
        }
    }

    /// Records a failure under its disposition; a safe-to-fall-back provider failure is reported
    /// as the target being unavailable before dispatch. An invalid request is never retryable,
    /// whatever disposition its source claimed.
    pub(crate) fn failed(disposition: AdapterAttemptDisposition, error: AdapterError) -> Self {
        let disposition = match error {
            AdapterError::InvalidRequest(_) => AdapterAttemptDisposition::NonRetryable,
            _ => disposition,
        };
        let error = match disposition {
            AdapterAttemptDisposition::SafeToFallback => pre_dispatch_error(error),
            AdapterAttemptDisposition::StartedOrAmbiguous
            | AdapterAttemptDisposition::NonRetryable => error,
        };
        Self::Failed { disposition, error }
    }

    pub(crate) fn allows_fallback(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                disposition: AdapterAttemptDisposition::SafeToFallback,
                ..
            }
        )
    }

    pub(crate) fn into_result(self) -> Result<AdapterResponse, AdapterError> {
        match self {
            Self::Response(response) => Ok(response),
            Self::Failed { error, .. } => Err(error),
        }
    }
}

/// Runs the adapter selected by the private route target; node-hosted targets go through the
/// manager's peer client, in process for the manager's own node and over the private listener
/// for any other node.
pub(crate) async fn invoke(
    target: SelectedInferenceTarget,
    request: InferenceRequest,
    cancellation: RequestCancellation,
    peers: Option<&peer::PeerInferenceClient>,
) -> AdapterAttempt {
    let model_id = request.model_id().to_owned();
    let (backend_id, deployment_id) = target_context(&target);
    let backend_id = backend_id.to_owned();
    let deployment_id = deployment_id.to_owned();
    let span = debug_span!(
        "inference_adapter",
        operation = "inference_adapter",
        model_id = %model_id,
        deployment_id = %deployment_id,
        backend = %backend_id,
        response_kind = field::Empty,
        upstream_status = field::Empty,
        outcome = field::Empty,
        error_kind = field::Empty,
    );
    let attempt = async move {
        match target {
            SelectedInferenceTarget::Peer(target) => match peers {
                Some(peers) => peer::invoke(peers, target, request, cancellation).await,
                None => peer_transport_unconfigured(),
            },
            SelectedInferenceTarget::Upstream(target) => match request {
                InferenceRequest::Chat(request) => AdapterAttempt::from_result(
                    openai_compatible::invoke(target, request, cancellation).await,
                ),
                // Upstream targets serve chat only, so nothing reached the provider.
                InferenceRequest::Decision(_) => adapter_attempt(
                    InferenceAttempt::unsupported_request_kind(),
                    adapter_response,
                ),
            },
        }
    }
    .instrument(span.clone())
    .await;
    record_adapter_attempt(&span, &attempt);
    attempt
}

/// Makes one attempt through a local target's own engine binding, refusing a mismatched identity.
pub(crate) async fn invoke_local_binding(
    target: LocalServingTarget,
    request: InferenceRequest,
    cancellation: RequestCancellation,
) -> AdapterAttempt {
    if target.binding_identity.deployment_id() != target.deployment_id.0
        || &target.binding_identity != target.binding.identity()
    {
        return AdapterAttempt::Failed {
            disposition: AdapterAttemptDisposition::StartedOrAmbiguous,
            error: AdapterError::upstream("engine binding identity does not match serving target"),
        };
    }
    let invoker = target.binding.invoker();
    match request {
        InferenceRequest::Chat(request) => adapter_attempt(
            invoker.invoke(request, cancellation).await,
            adapter_response,
        ),
        InferenceRequest::Decision(request) => adapter_attempt(
            invoker.invoke_decision(request, cancellation).await,
            adapter_decision_response,
        ),
    }
}

/// Refuses a node-hosted target before any dispatch on a manager without a private listener,
/// so the router may safely try the route's next fallback group.
fn peer_transport_unconfigured() -> AdapterAttempt {
    AdapterAttempt::failed(
        AdapterAttemptDisposition::SafeToFallback,
        AdapterError::PreDispatchUnavailable(anyhow::anyhow!("peer transport is not configured")),
    )
}

fn adapter_attempt<R>(
    attempt: InferenceAttempt<R>,
    response: fn(R) -> AdapterResponse,
) -> AdapterAttempt {
    match attempt {
        InferenceAttempt::Response(result) => AdapterAttempt::Response(response(result)),
        InferenceAttempt::Failed { disposition, error } => AdapterAttempt::failed(
            match disposition {
                InferenceAttemptDisposition::SafeToFallback => {
                    AdapterAttemptDisposition::SafeToFallback
                }
                InferenceAttemptDisposition::StartedOrAmbiguous => {
                    AdapterAttemptDisposition::StartedOrAmbiguous
                }
                InferenceAttemptDisposition::NonRetryable => {
                    AdapterAttemptDisposition::NonRetryable
                }
            },
            adapter_error(error),
        ),
    }
}

fn pre_dispatch_error(error: AdapterError) -> AdapterError {
    match error {
        AdapterError::Upstream(error) => AdapterError::PreDispatchUnavailable(error),
        error => error,
    }
}

fn adapter_response(response: InferenceResponse) -> AdapterResponse {
    match response {
        InferenceResponse::Complete(response) => AdapterResponse::Complete(response),
        InferenceResponse::Stream(mut stream) => {
            let (sender, receiver) = mpsc::channel(8);
            tokio::spawn(async move {
                while let Some(event) = stream.recv().await {
                    if sender.send(event.map_err(adapter_error)).await.is_err() {
                        return;
                    }
                }
            });
            AdapterResponse::Stream(receiver)
        }
        InferenceResponse::Passthrough(InferencePassthroughResponse {
            status,
            headers,
            body,
        }) => AdapterResponse::Upstream(UpstreamResponse {
            status,
            headers,
            body,
        }),
    }
}

fn adapter_decision_response(response: DecisionResponse) -> AdapterResponse {
    match response {
        DecisionResponse::Complete(result) => AdapterResponse::Decision(result),
        DecisionResponse::Passthrough(InferencePassthroughResponse {
            status,
            headers,
            body,
        }) => AdapterResponse::Upstream(UpstreamResponse {
            status,
            headers,
            body,
        }),
    }
}

fn adapter_error(error: InferenceError) -> AdapterError {
    match error {
        InferenceError::Cancelled(reason) => AdapterError::Cancelled(reason),
        InferenceError::Provider(error) => AdapterError::Upstream(error),
        InferenceError::UnsupportedCapability => AdapterError::UnsupportedCapability,
        // The engine refused before any work, so the target was unavailable for this request.
        error @ InferenceError::UnsupportedRequestKind => {
            AdapterError::PreDispatchUnavailable(anyhow::Error::new(error))
        }
        InferenceError::InvalidRequest(error) => AdapterError::InvalidRequest(error),
    }
}

/// Shared OpenAI-compatible adapter transport and wire-protocol failure.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdapterTransportError {
    #[error(transparent)]
    Codec(#[from] crate::router::openai_compatible_codec::CodecError),

    #[error("adapter transport completion decoding error: {0}")]
    CompletionDecode(#[source] crate::router::openai_compatible_codec::CodecError),

    #[error("adapter transport SSE decoding error: {0}")]
    StreamDecode(#[source] crate::router::openai_compatible_codec::CodecError),

    #[error("adapter transport decision decoding error: {0}")]
    DecisionDecode(#[source] anyhow::Error),

    #[error("adapter transport stream terminal policy error: {0}")]
    StreamPolicy(#[source] crate::router::openai_compatible_codec::CodecError),

    #[error("adapter transport limit exceeded: {0}")]
    Limit(&'static str),

    #[error("adapter transport connection error: {0}")]
    Connect(#[source] std::io::Error),

    #[error("adapter transport I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("adapter transport JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("adapter transport UTF-8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("adapter transport integer parse error: {0}")]
    Integer(#[from] std::num::ParseIntError),

    #[error("adapter transport header-name error: {0}")]
    HeaderName(#[from] http::header::InvalidHeaderName),

    #[error("adapter transport header-value error: {0}")]
    HeaderValue(#[from] http::header::InvalidHeaderValue),

    #[error("adapter transport header text error: {0}")]
    HeaderText(#[from] http::header::ToStrError),

    #[error("invalid adapter transport response: {0}")]
    InvalidResponse(#[source] anyhow::Error),
}

impl AdapterTransportError {
    /// Safe diagnostic stage of this failure, such as `connect` or `sse_decoding`.
    fn stage(&self) -> &'static str {
        match self {
            Self::Codec(_) | Self::Json(_) => "response_decoding",
            Self::CompletionDecode(_) => "completion_decoding",
            Self::StreamDecode(_) => "sse_decoding",
            Self::DecisionDecode(_) => "decision_decoding",
            Self::StreamPolicy(_) => "terminal_policy",
            Self::Limit(_) => "limit",
            Self::Connect(_) => "connect",
            Self::Io(_) => "transport_io",
            Self::Utf8(_)
            | Self::Integer(_)
            | Self::HeaderName(_)
            | Self::HeaderValue(_)
            | Self::HeaderText(_)
            | Self::InvalidResponse(_) => "http_framing",
        }
    }
}

/// Classifies an adapter failure into a safe diagnostic stage without rendering its sources.
pub(crate) fn failure_stage(error: &AdapterError) -> &'static str {
    let source = match error {
        AdapterError::Upstream(source) | AdapterError::PreDispatchUnavailable(source) => source,
        AdapterError::Cancelled(_) => return "cancelled",
        AdapterError::UnsupportedCapability => return "unsupported_capability",
        AdapterError::InvalidRequest(_) => return "invalid_request",
    };
    for cause in source.chain() {
        if let Some(error) = cause.downcast_ref::<AdapterTransportError>() {
            return error.stage();
        }
        if let Some(InferenceError::UnsupportedRequestKind) = cause.downcast_ref::<InferenceError>()
        {
            return "unsupported_request_kind";
        }
        if let Some(error) = cause.downcast_ref::<reqwest::Error>() {
            return if error.is_connect() {
                "connect"
            } else if error.is_timeout() {
                "timeout"
            } else {
                "transport"
            };
        }
        if cause.is::<crate::router::openai_compatible_codec::CodecError>() {
            return "response_decoding";
        }
        if cause.is::<std::io::Error>() {
            return "transport_io";
        }
    }
    "upstream"
}
/// A typed adapter failure that keeps cancellation separate from upstream faults.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AdapterError {
    #[error("inference adapter request cancelled: {0}")]
    Cancelled(RequestCancellationReason),

    #[error("inference adapter upstream failed: {0}")]
    Upstream(#[source] anyhow::Error),

    #[error("inference adapter target was unavailable before dispatch: {0}")]
    PreDispatchUnavailable(#[source] anyhow::Error),

    #[error("inference adapter request exceeds the selected target capabilities")]
    UnsupportedCapability,

    #[error("inference adapter request is invalid: {0}")]
    InvalidRequest(InvalidRequest),
}

impl From<AdapterTransportError> for AdapterError {
    fn from(error: AdapterTransportError) -> Self {
        Self::Upstream(anyhow::Error::new(error))
    }
}

/// Unchanged unsuccessful llama.cpp response evidence for public pass-through.
pub(crate) struct UpstreamResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn failure_disposition(error: &AdapterError) -> AdapterAttemptDisposition {
    match error {
        AdapterError::PreDispatchUnavailable(_) => AdapterAttemptDisposition::SafeToFallback,
        AdapterError::Cancelled(_)
        | AdapterError::Upstream(_)
        | AdapterError::UnsupportedCapability => AdapterAttemptDisposition::StartedOrAmbiguous,
        AdapterError::InvalidRequest(_) => AdapterAttemptDisposition::NonRetryable,
    }
}

fn record_adapter_attempt(span: &Span, attempt: &AdapterAttempt) {
    match attempt {
        AdapterAttempt::Response(AdapterResponse::Complete(_)) => {
            span.record("response_kind", "complete");
            span.record("outcome", "success");
        }
        AdapterAttempt::Response(AdapterResponse::Stream(_)) => {
            span.record("response_kind", "stream");
            span.record("outcome", "streaming");
        }
        AdapterAttempt::Response(AdapterResponse::Decision(_)) => {
            span.record("response_kind", "decision");
            span.record("outcome", "success");
        }
        AdapterAttempt::Response(AdapterResponse::Upstream(response)) => {
            span.record("response_kind", "upstream");
            span.record("outcome", "failed");
            span.record("error_kind", "upstream_status");
            span.record("upstream_status", response.status);
            // The engine's body may echo the request, so only its status is recorded.
            warn!(
                parent: span,
                error_kind = "upstream_status",
                failure_stage = "upstream_status",
                upstream_status = response.status,
                "inference engine rejected the request",
            );
        }
        AdapterAttempt::Failed {
            error: AdapterError::Cancelled(reason),
            ..
        } => {
            span.record("outcome", "cancelled");
            span.record("error_kind", format!("{reason:?}"));
        }
        AdapterAttempt::Failed {
            error: error @ AdapterError::Upstream(_),
            ..
        } => {
            span.record("outcome", "failed");
            span.record("error_kind", "upstream");
            error!(
                parent: span,
                error_kind = "upstream",
                failure_stage = failure_stage(error),
                "inference adapter failed",
            );
        }
        AdapterAttempt::Failed {
            error: AdapterError::InvalidRequest(invalid),
            ..
        } => {
            // A caller error, never fallback-eligible: attributable but not an engine fault.
            span.record("outcome", "rejected");
            span.record("error_kind", "invalid_request");
            warn!(
                parent: span,
                error_kind = "invalid_request",
                category = ?invalid.category,
                param = invalid.param.unwrap_or("unspecified"),
                "inference adapter refused an invalid request",
            );
        }
        AdapterAttempt::Failed {
            error: error @ AdapterError::PreDispatchUnavailable(_),
            ..
        } => {
            // The router may still serve the request from the route's next fallback group.
            span.record("outcome", "unavailable");
            span.record("error_kind", "pre_dispatch_unavailable");
            warn!(
                parent: span,
                error_kind = "pre_dispatch_unavailable",
                failure_stage = failure_stage(error),
                "inference adapter target unavailable before dispatch",
            );
        }
        AdapterAttempt::Failed {
            error: AdapterError::UnsupportedCapability,
            ..
        } => {
            span.record("outcome", "rejected");
            span.record("error_kind", "unsupported_capability");
        }
    }
}

fn target_context(target: &SelectedInferenceTarget) -> (&'static str, &str) {
    match target {
        SelectedInferenceTarget::Peer(target) => ("peer", &target.deployment_id.0),
        SelectedInferenceTarget::Upstream(target) => {
            ("upstream/unavailable", &target.deployment_id.0)
        }
    }
}
impl AdapterError {
    pub(crate) fn upstream(message: &str) -> Self {
        Self::Upstream(anyhow::anyhow!(message.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::CatalogModelId;
    use dotcore_domain::test_support::tracing::{Record, capture_records, capture_records_async};
    use dotcore_proto::dotcore::inference::v1::{
        ChatContentPart, ChatImageContent, ChatImageMediaType, ChatMessage, ChatMessageRole,
        ChatRequest, chat_content_part, chat_message, chat_stream_event::Event,
    };
    use dotcore_runtime::LoopbackEndpoint;
    use serde_json::Value;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{mpsc, watch},
        time::{Duration, timeout},
    };

    use super::*;
    use crate::{
        model::{
            controller::serving_targets::{LocalServingTarget, ServingTargetsRevocationReason},
            domain::ModelDeploymentId,
        },
        router::{
            cancellation::RequestCancellationReason,
            openai::{self, OpenAiRequest},
            selection::SelectedInferenceTarget,
            test_fixtures,
        },
    };

    #[tokio::test]
    async fn local_adapter_accepts_binding_revision_independent_of_reconcile_generation() {
        let (response, _) = invoke_docker(
            decode_chat_request(&test_fixtures::chat_request()),
            json_bytes(&test_fixtures::completion()),
            "application/json",
        )
        .await;

        assert!(matches!(response, AdapterResponse::Complete(_)));
    }

    #[tokio::test]
    async fn local_adapter_rejects_an_identity_different_from_its_binding() {
        let mut target = local_target(50000);
        target.binding_identity = crate::inference::EngineBindingIdentity::new(
            "docker-fixture",
            "fixture-realization",
            "http",
            50001,
        )
        .expect("different binding identity");

        let attempt = invoke_local_binding(
            target,
            InferenceRequest::Chat(decode_chat_request(&test_fixtures::chat_request())),
            uncancelled(),
        )
        .await;
        let AdapterAttempt::Failed { error, .. } = attempt else {
            panic!("mismatched target identity must fail");
        };

        assert_eq!(
            error.to_string(),
            "inference adapter upstream failed: engine binding identity does not match serving target"
        );
    }

    #[tokio::test]
    async fn shared_chat_fixture_preserves_adapter_dispatch_fidelity() {
        let request = decode_chat_request(&test_fixtures::chat_request());
        let (response, llama_request_body) = invoke_docker(
            request.clone(),
            json_bytes(&test_fixtures::completion()),
            "application/json",
        )
        .await;
        assert!(matches!(response, AdapterResponse::Complete(_)));
        assert_eq!(
            serde_json::from_slice::<Value>(&llama_request_body).expect("llama request JSON"),
            oracle_for_model(&test_fixtures::llama_request_oracle(), "fixture-model")
        );

        let mut stream_request_value = test_fixtures::chat_request();
        stream_request_value
            .as_object_mut()
            .expect("request object")
            .insert("stream".to_owned(), Value::Bool(true));
        let stream_request = decode_chat_request(&stream_request_value);
        let expected_events = crate::router::openai_compatible_codec::decode_sse(
            &test_fixtures::fragmented_stream_body(),
        )
        .expect("fragmented SSE");
        let (response, _) = invoke_docker(
            stream_request.clone(),
            test_fixtures::fragmented_stream_body(),
            "text/event-stream",
        )
        .await;
        let AdapterResponse::Stream(adapter_events) = response else {
            panic!("llama.cpp adapter must return a stream")
        };
        assert_eq!(collect_events(adapter_events).await, expected_events);
    }

    #[tokio::test]
    async fn openai_compatible_adapter_rewrites_only_the_outbound_model_and_preserves_chat_wire_fields()
     {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let request = read_http_request(&mut stream).await;
            request_sender
                .send(request)
                .expect("captured request receiver");
            let mut completion = test_fixtures::completion();
            completion
                .as_object_mut()
                .expect("completion object")
                .insert(
                    "model".to_owned(),
                    Value::String("private-model".to_owned()),
                );
            let body = json_bytes(&completion);
            write_http_response(&mut stream, 200, "application/json", &body).await;
        });
        let request = decode_chat_request(&test_fixtures::chat_request());
        let response = invoke(upstream_target(port, false), request, uncancelled(), None)
            .await
            .into_result()
            .expect("OpenAI-compatible response");
        let captured = request_receiver.await.expect("captured request");
        upstream.await.expect("upstream task");

        assert_eq!(captured.method_path, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(captured.header("authorization"), Some("Bearer test-secret"));
        assert_eq!(captured.header("content-type"), Some("application/json"));
        assert_eq!(
            captured.header("accept"),
            Some("application/json, text/event-stream")
        );
        // Upstream providers are always asked not to store the request, even when the public
        // request omitted `store`.
        let mut expected =
            oracle_for_model(&test_fixtures::llama_request_oracle(), "private-model");
        expected
            .as_object_mut()
            .expect("OpenAI-compatible request")
            .insert("store".to_owned(), Value::Bool(false));
        assert_eq!(
            serde_json::from_slice::<Value>(&captured.body).expect("OpenAI-compatible JSON"),
            expected
        );
        let AdapterResponse::Complete(completion) = response else {
            panic!("OpenAI-compatible adapter must return a completion")
        };
        assert_eq!(completion.model_id, "fixture-route");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_decodes_sse_and_preserves_the_public_route_name() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let _request = read_http_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .expect("upstream headers");
            stream
                .write_all(&test_fixtures::fragmented_stream_body())
                .await
                .expect("upstream stream");
        });
        let mut request = decode_chat_request(&test_fixtures::chat_request());
        request.stream = Some(true);
        let response = invoke(upstream_target(port, false), request, uncancelled(), None)
            .await
            .into_result()
            .expect("OpenAI-compatible stream");
        let AdapterResponse::Stream(events) = response else {
            panic!("OpenAI-compatible adapter must return a stream")
        };
        let events = collect_events(events).await;
        upstream.await.expect("upstream task");

        assert_eq!(
            events.iter().find_map(|event| match &event.event {
                Some(Event::Started(started)) => Some(started.model_id.as_str()),
                _ => None,
            }),
            Some("fixture-route")
        );
        assert!(matches!(
            events.last().and_then(|event| event.event.as_ref()),
            Some(Event::Completed(_))
        ));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_encodes_images_only_for_targets_declaring_image_support() {
        assert!(matches!(
            invoke(
                upstream_target(9, false),
                image_request(),
                uncancelled(),
                None
            )
            .await,
            AdapterAttempt::Failed {
                error: AdapterError::UnsupportedCapability,
                ..
            }
        ));

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            request_sender
                .send(read_http_request(&mut stream).await)
                .expect("captured request receiver");
            write_http_response(
                &mut stream,
                200,
                "application/json",
                &json_bytes(&test_fixtures::completion()),
            )
            .await;
        });
        let _response = invoke(
            upstream_target(port, true),
            image_request(),
            uncancelled(),
            None,
        )
        .await
        .into_result()
        .expect("image-capable upstream response");
        let captured = request_receiver.await.expect("captured request");
        upstream.await.expect("upstream task");
        assert_eq!(
            serde_json::from_slice::<Value>(&captured.body)
                .expect("OpenAI-compatible JSON")
                .pointer("/messages/0/content/0/image_url/url"),
            Some(&Value::String("data:image/png;base64,AQI=".to_owned()))
        );
    }

    #[tokio::test]
    async fn openai_compatible_adapter_does_not_retry_a_connect_failure() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let port = reserved.port();

        let attempt = invoke(
            upstream_target(port, false),
            ChatRequest {
                model_id: "fixture-model".to_owned(),
                ..Default::default()
            },
            uncancelled(),
            None,
        )
        .await;
        let AdapterAttempt::Failed {
            error: AdapterError::Upstream(source),
            ..
        } = attempt
        else {
            panic!("connection failure must retain its HTTP-client source");
        };
        assert!(source.downcast_ref::<reqwest::Error>().is_some());
    }

    #[tokio::test]
    async fn openai_compatible_adapter_makes_one_attempt_for_upstream_failures_and_denied_redirects()
     {
        for (status, location) in [(429, None), (503, None), (302, Some("/redirected"))] {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("upstream listener");
            let port = listener.local_addr().expect("upstream address").port();
            let (attempt_sender, attempt_receiver) = tokio::sync::oneshot::channel();
            let upstream = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("upstream connection");
                let _request = read_http_request(&mut stream).await;
                attempt_sender.send(()).expect("attempt receiver");
                let location = location
                    .map(|value| format!("Location: {value}\r\n"))
                    .unwrap_or_default();
                let body = b"{\"error\":\"private body\"}";
                let response = format!(
                    "HTTP/1.1 {status} Fixture\r\n{location}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).expect("error JSON"),
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            });
            let response = invoke(
                upstream_target(port, false),
                ChatRequest {
                    model_id: "fixture-model".to_owned(),
                    ..Default::default()
                },
                uncancelled(),
                None,
            )
            .await
            .into_result()
            .expect("upstream status response");
            attempt_receiver.await.expect("one outbound attempt");
            upstream.await.expect("upstream task");

            assert!(matches!(
                response,
                AdapterResponse::Upstream(UpstreamResponse { status: actual, .. }) if actual == status
            ));
        }
    }

    #[tokio::test]
    async fn openai_compatible_adapter_rejects_data_after_done_through_eof() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let _request = read_http_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n")
                .await
                .expect("upstream headers");
            stream
                .write_all(&test_fixtures::fragmented_stream_body())
                .await
                .expect("valid upstream stream");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = stream
                .write_all(b"data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"late\"},\"finish_reason\":null}]}\n\n")
                .await;
        });
        let response = invoke(
            upstream_target(port, false),
            ChatRequest {
                model_id: "fixture-model".to_owned(),
                stream: Some(true),
                ..Default::default()
            },
            uncancelled(),
            None,
        )
        .await
        .into_result()
        .expect("OpenAI-compatible stream");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("OpenAI-compatible adapter must return a stream")
        };
        let mut completed = false;
        let mut failed = false;
        while let Some(event) = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("adapter stream event timed out")
        {
            match event {
                Ok(event) => completed |= matches!(event.event, Some(Event::Completed(_))),
                Err(AdapterError::Upstream(_)) => failed = true,
                Err(error) => panic!("unexpected stream error: {error}"),
            }
        }
        upstream.await.expect("upstream task");

        assert!(failed, "post-[DONE] bytes must fail the stream");
        assert!(!completed, "failed streams must not complete");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_rejects_an_oversized_complete_frame_before_decoding() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let _request = read_http_request(&mut stream).await;
            let oversized = serde_json::json!({
                "id": "chatcmpl_oversized",
                "model": "private-model",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": "x".repeat(super::openai_compatible::MAX_SSE_FRAME_BYTES),
                    },
                    "finish_reason": null,
                }],
            });
            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                serde_json::to_string(&oversized).expect("oversized stream frame")
            );
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(headers.as_bytes())
                .await
                .expect("upstream headers");
            stream
                .write_all(body.as_bytes())
                .await
                .expect("oversized upstream stream");
            stream.shutdown().await.expect("upstream shutdown");
        });
        let response = invoke(
            upstream_target(port, false),
            ChatRequest {
                model_id: "fixture-model".to_owned(),
                stream: Some(true),
                ..Default::default()
            },
            uncancelled(),
            None,
        )
        .await
        .into_result()
        .expect("OpenAI-compatible stream");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("OpenAI-compatible adapter must return a stream")
        };
        let mut failed = false;
        let mut completed = false;
        while let Some(event) = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("adapter stream event timed out")
        {
            match event {
                Ok(event) => completed |= matches!(event.event, Some(Event::Completed(_))),
                Err(AdapterError::Upstream(_)) => failed = true,
                Err(error) => panic!("unexpected stream error: {error}"),
            }
        }
        upstream.await.expect("upstream task");

        assert!(failed, "complete oversized SSE frames must be rejected");
        assert!(!completed, "failed streams must not complete");
    }

    #[tokio::test]
    async fn openai_compatible_adapter_cancels_before_dispatch_when_the_client_disconnects() {
        let (cancellation, client, _route) = cancellation_with_senders();
        client.send_replace(Some(RequestCancellationReason::ClientDisconnected));

        assert!(matches!(
            invoke(
                upstream_target(9, false),
                ChatRequest {
                    model_id: "fixture-model".to_owned(),
                    ..Default::default()
                },
                cancellation,
                None,
            )
            .await,
            AdapterAttempt::Failed {
                error: AdapterError::Cancelled(RequestCancellationReason::ClientDisconnected),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_cancels_a_withdrawn_stream_without_a_completed_event() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let first_frame_sent = std::sync::Arc::new(tokio::sync::Barrier::new(2));
        let upstream = tokio::spawn({
            let first_frame_sent = first_frame_sent.clone();
            async move {
                let (mut stream, _) = listener.accept().await.expect("upstream connection");
                let _request = read_http_request(&mut stream).await;
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n").await.expect("upstream headers");
                stream.write_all(b"data: {\"id\":\"chatcmpl_upstream\",\"model\":\"private-model\",\"created\":0,\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"one\"},\"finish_reason\":null}]}\n\n").await.expect("first frame");
                timeout(Duration::from_secs(1), first_frame_sent.wait())
                    .await
                    .expect("first frame barrier timed out");
                std::future::pending::<()>().await;
            }
        });
        let (cancellation, _client, route) = cancellation_with_senders();
        let response = invoke(
            upstream_target(port, false),
            ChatRequest {
                model_id: "fixture-model".to_owned(),
                stream: Some(true),
                ..Default::default()
            },
            cancellation,
            None,
        )
        .await
        .into_result()
        .expect("stream response");
        let AdapterResponse::Stream(mut events) = response else {
            panic!("OpenAI-compatible adapter must return a stream")
        };
        timeout(Duration::from_secs(1), first_frame_sent.wait())
            .await
            .expect("first frame barrier timed out");
        let forwarded = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("first event timed out")
            .expect("first event")
            .expect("valid event");
        assert!(matches!(forwarded.event, Some(Event::Started(_))));
        route.send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));
        let mut completed = false;
        loop {
            match timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("stream cancellation timed out")
                .expect("stream cancellation")
            {
                Ok(event) => completed |= matches!(event.event, Some(Event::Completed(_))),
                Err(cancellation) => {
                    assert!(matches!(
                        cancellation,
                        AdapterError::Cancelled(RequestCancellationReason::RouteRevoked)
                    ));
                    break;
                }
            }
        }
        assert!(!completed);
        assert!(
            timeout(Duration::from_secs(1), events.recv())
                .await
                .expect("stream close timed out")
                .is_none()
        );
        upstream.abort();
    }

    #[test]
    fn adapter_variants_record_safe_terminal_span_fields() {
        let (_, records, output) = capture_records(|| {
            let complete = AdapterAttempt::Response(AdapterResponse::Complete(
                ChatCompletionResult::default(),
            ));
            let (_, stream) = mpsc::channel(1);
            let streaming = AdapterAttempt::Response(AdapterResponse::Stream(stream));
            let upstream = AdapterAttempt::Response(AdapterResponse::Upstream(UpstreamResponse {
                status: 503,
                headers: Vec::new(),
                body: Vec::new(),
            }));
            for result in [complete, streaming, upstream] {
                let span = debug_span!(
                    "inference_adapter",
                    operation = "inference_adapter",
                    model_id = "model",
                    deployment_id = "deployment",
                    backend = "docker",
                    response_kind = field::Empty,
                    upstream_status = field::Empty,
                    outcome = field::Empty,
                    error_kind = field::Empty,
                );
                span.in_scope(|| record_adapter_attempt(&span, &result));
            }
            let failure =
                AdapterAttempt::from_result(Err(AdapterError::upstream("secret upstream details")));
            let span = debug_span!(
                "inference_adapter",
                operation = "inference_adapter",
                model_id = "model",
                deployment_id = "deployment",
                backend = "docker",
                response_kind = field::Empty,
                upstream_status = field::Empty,
                outcome = field::Empty,
                error_kind = field::Empty,
            );
            span.in_scope(|| record_adapter_attempt(&span, &failure));
            let refusal = AdapterAttempt::failed(
                AdapterAttemptDisposition::SafeToFallback,
                AdapterError::PreDispatchUnavailable(anyhow::anyhow!("secret refusal details")),
            );
            let span = debug_span!(
                "inference_adapter",
                operation = "inference_adapter",
                model_id = "model",
                deployment_id = "deployment",
                backend = "peer",
                response_kind = field::Empty,
                upstream_status = field::Empty,
                outcome = field::Empty,
                error_kind = field::Empty,
            );
            span.in_scope(|| record_adapter_attempt(&span, &refusal));
        });

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("response_kind").is_some_and(|value| value.contains("complete"))
                    && fields.get("outcome").is_some_and(|value| value.contains("success"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("response_kind").is_some_and(|value| value.contains("stream"))
                    && fields.get("outcome").is_some_and(|value| value.contains("streaming"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("response_kind").is_some_and(|value| value.contains("upstream"))
                    && fields.get("upstream_status").is_some_and(|value| value.contains("503"))
                    && fields.get("outcome").is_some_and(|value| value.contains("failed"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("error_kind").is_some_and(|value| value.contains("upstream"))
                    && fields.get("outcome").is_some_and(|value| value.contains("failed"))
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "ERROR"
                    && fields.get("error_kind").is_some_and(|value| value.contains("upstream"))
        )));
        assert!(!output.contains("secret upstream details"));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("error_kind").is_some_and(|value| value.contains("pre_dispatch_unavailable"))
                    && fields.get("outcome").is_some_and(|value| value.contains("unavailable"))
        )));
        let refusal_levels = records
            .iter()
            .filter_map(|record| match record {
                Record::Event { level, fields }
                    if fields
                        .get("error_kind")
                        .is_some_and(|value| value.contains("pre_dispatch_unavailable")) =>
                {
                    Some(level.as_str())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            refusal_levels,
            ["WARN"],
            "a refusal the route may fall back from is a warning, not an error"
        );
        assert!(!output.contains("secret refusal details"));
    }

    #[test]
    fn adapter_failure_retains_codec_and_json_sources() {
        let json_error = serde_json::from_str::<Value>("{").expect_err("invalid JSON");
        let codec_error = openai_compatible_codec::CodecError::Json(json_error);
        let transport_error = AdapterTransportError::from(codec_error);
        let error = AdapterError::from(transport_error);

        let AdapterError::Upstream(source) = error else {
            panic!("adapter must retain typed transport, codec, and JSON errors");
        };
        assert!(source.chain().any(|cause| cause.is::<serde_json::Error>()));
    }

    #[tokio::test]
    async fn production_adapter_span_declares_upstream_status_field() {
        let (_, records, _) = capture_records_async(|| async {
            let result = invoke(
                upstream_target(1, false),
                ChatRequest {
                    model_id: "model".to_owned(),
                    ..Default::default()
                },
                cancelled(),
                None,
            )
            .await
            .into_result();
            assert!(matches!(
                result,
                Err(AdapterError::Cancelled(
                    RequestCancellationReason::ClientDisconnected
                ))
            ));
        })
        .await;

        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanCreated { name, fields }
                if name == "inference_adapter" && fields.contains_key("upstream_status")
        )));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::SpanClosed { name, fields }
                if name == "inference_adapter"
                    && fields.get("outcome").is_some_and(|value| value.contains("cancelled"))
        )));
    }

    #[tokio::test]
    async fn openai_compatible_adapter_forwards_vllm_controls_with_store_false_under_its_private_identity()
     {
        let (port, upstream) = serve_upstream_once(
            200,
            "application/json",
            json_bytes(&test_fixtures::completion()),
        )
        .await;
        let OpenAiRequest::Chat(request) = openai::decode(
            "POST",
            "/v1/chat/completions",
            br#"{"model":"fixture-route","messages":[{"role":"user","content":"hi"}],"reasoning_effort":"high","chat_template_kwargs":{"enable_thinking":true,"preserve_thinking":true},"top_k":-1,"n":2,"seed":184467440737095516160}"#,
        )
        .expect("Chat request") else {
            panic!("fixture must decode as Chat")
        };

        invoke(upstream_target(port, false), request, uncancelled(), None)
            .await
            .into_result()
            .expect("upstream completion");

        let captured = upstream.await.expect("upstream task");
        let text = String::from_utf8(captured.body.clone()).expect("UTF-8 request body");
        assert!(text.contains(r#""seed":184467440737095516160"#));
        let mut sent = serde_json::from_str::<Value>(&text).expect("request JSON");
        sent.as_object_mut().expect("request object").remove("seed");
        assert_eq!(
            sent,
            serde_json::json!({
                "model": "private-model",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": false,
                "store": false,
                "reasoning_effort": "high",
                "chat_template_kwargs": {"enable_thinking": true, "preserve_thinking": true},
                "top_k": -1,
                "n": 2
            })
        );
        assert_eq!(captured.method_path, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(captured.header("authorization"), Some("Bearer test-secret"));
    }

    #[tokio::test]
    async fn caller_controls_cannot_redirect_or_reauthorize_an_upstream_attempt() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        for key in [
            "model",
            "authorization",
            "api_key",
            "base_url",
            "headers",
            "endpoint",
        ] {
            let request = ChatRequest {
                model_id: "fixture-route".to_owned(),
                request_extensions_json: Some(format!(r#"{{"{key}":"http://evil.test"}}"#)),
                ..Default::default()
            };

            let attempt = invoke(upstream_target(port, false), request, uncancelled(), None).await;

            assert!(!attempt.allows_fallback(), "{key}");
            assert!(
                matches!(
                    attempt,
                    AdapterAttempt::Failed {
                        disposition: AdapterAttemptDisposition::NonRetryable,
                        error: AdapterError::InvalidRequest(_),
                    }
                ),
                "{key}"
            );
        }
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "no provider request is made for a refused control"
        );
    }

    #[tokio::test]
    async fn adapter_failures_emit_one_attributable_event_without_payload_values() {
        const SENTINELS: &[&str] = &[
            "prompt-sentinel",
            "reasoning-sentinel",
            "tool-args-sentinel",
            "sentinel_key",
            "extension-sentinel",
            "test-secret",
            "private-model",
        ];
        let request = || ChatRequest {
            model_id: "fixture-route".to_owned(),
            messages: vec![
                ChatMessage {
                    role: ChatMessageRole::User as i32,
                    content: Some(chat_message::Content::TextContent(
                        "prompt-sentinel".to_owned(),
                    )),
                    ..Default::default()
                },
                ChatMessage {
                    role: ChatMessageRole::Assistant as i32,
                    reasoning: Some("reasoning-sentinel".to_owned()),
                    tool_calls: vec![dotcore_proto::dotcore::inference::v1::FunctionToolCall {
                        tool_call_id: "call".to_owned(),
                        function_name: "read".to_owned(),
                        arguments_json: "tool-args-sentinel".to_owned(),
                    }],
                    ..Default::default()
                },
            ],
            request_extensions_json: Some(r#"{"sentinel_key":"extension-sentinel"}"#.to_owned()),
            ..Default::default()
        };
        let refused = dotcore_domain::test_support::ports::reserve_port();
        let refused_port = refused.port();

        let (ports, records, output) = capture_records_async(|| async {
            let (rejected_port, rejected) = serve_upstream_once(
                400,
                "application/json",
                br#"{"error":{"message":"prompt-sentinel is not allowed","param":"sentinel_key"}}"#
                    .to_vec(),
            )
            .await;
            let attempt = invoke(upstream_target(rejected_port, false), request(), uncancelled(), None).await;
            assert!(matches!(
                attempt.into_result(),
                Ok(AdapterResponse::Upstream(UpstreamResponse { status: 400, .. }))
            ));
            rejected.await.expect("upstream task");

            let (malformed_port, malformed) = serve_upstream_once(
                200,
                "application/json",
                br#"{"id":"c","model":"private-model","created":1,"choices":[{"index":0,"message":{"role":"assistant","content":7},"finish_reason":"stop"}]}"#
                    .to_vec(),
            )
            .await;
            let attempt = invoke(upstream_target(malformed_port, false), request(), uncancelled(), None).await;
            assert!(matches!(attempt.into_result(), Err(AdapterError::Upstream(_))));
            malformed.await.expect("upstream task");

            let attempt = invoke(upstream_target(refused_port, false), request(), uncancelled(), None).await;
            let Err(error) = attempt.into_result() else {
                panic!("a refused connection must fail");
            };
            for sentinel in SENTINELS {
                assert!(!format!("{error:#}").contains(sentinel), "{sentinel}");
            }

            let mut invalid = request();
            invalid.request_extensions_json =
                Some(r#"{"base_url":"extension-sentinel"}"#.to_owned());
            let attempt = invoke(upstream_target(refused_port, false), invalid, uncancelled(), None).await;
            assert!(matches!(attempt.into_result(), Err(AdapterError::InvalidRequest(_))));
            [rejected_port, malformed_port]
        })
        .await;

        let failures = records
            .iter()
            .filter_map(|record| match record {
                Record::Event { level, fields } if fields.contains_key("error_kind") => Some((
                    level.as_str(),
                    fields.get("failure_stage").map(String::as_str),
                    fields.get("error_kind").map(String::as_str),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            failures,
            [
                (
                    "WARN",
                    Some("\"upstream_status\""),
                    Some("\"upstream_status\"")
                ),
                (
                    "ERROR",
                    Some("\"completion_decoding\""),
                    Some("\"upstream\"")
                ),
                ("ERROR", Some("\"connect\""), Some("\"upstream\"")),
                ("WARN", None, Some("\"invalid_request\"")),
            ]
        );
        // Dependency transport internals log endpoints at debug level, below the backend's
        // default filter; this checks the lines the backend itself emits.
        let backend_output = output
            .lines()
            .filter(|line| line.contains(": dotcore::"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(backend_output.contains("failure_stage=\"completion_decoding\""));
        for sentinel in SENTINELS
            .iter()
            .map(|sentinel| (*sentinel).to_owned())
            .chain(ports.iter().map(|port| format!("127.0.0.1:{port}")))
            .chain([format!("127.0.0.1:{refused_port}")])
        {
            assert!(
                !backend_output.contains(&sentinel),
                "{sentinel} leaked into logs:\n{backend_output}"
            );
        }
    }

    #[tokio::test]
    async fn upstream_stream_failures_are_classified_by_safe_stage() {
        let start = "data: {\"id\":\"c\",\"model\":\"m\",\"created\":1,\"choices\":[{\"index\":0,\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n";
        let stop =
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";
        for (body, include_usage, stage) in [
            (
                format!("{start}data: {{not json}}\n\n"),
                false,
                "sse_decoding",
            ),
            (
                format!("{start}{stop}data: [DONE]\n\n"),
                true,
                "terminal_policy",
            ),
            (
                format!(
                    "{start}data: {}",
                    "x".repeat(openai_compatible::MAX_SSE_FRAME_BYTES + 1)
                ),
                false,
                "limit",
            ),
        ] {
            let (port, upstream) =
                serve_upstream_once(200, "text/event-stream", body.into_bytes()).await;
            let request = ChatRequest {
                model_id: "fixture-route".to_owned(),
                stream: Some(true),
                include_stream_usage: Some(include_usage),
                ..Default::default()
            };

            let Ok(AdapterResponse::Stream(mut events)) =
                invoke(upstream_target(port, false), request, uncancelled(), None)
                    .await
                    .into_result()
            else {
                panic!("{stage} stream must start");
            };
            let error = loop {
                match timeout(Duration::from_secs(1), events.recv())
                    .await
                    .expect("stream item")
                {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error,
                    None => panic!("{stage} stream must fail"),
                }
            };
            assert_eq!(failure_stage(&error), stage);
            upstream.abort();
        }
    }

    #[tokio::test]
    async fn a_decision_a_target_does_not_serve_is_refused_before_dispatch_and_falls_back() {
        let decision = || {
            InferenceRequest::Decision(dotcore_proto::dotcore::inference::v1::DecisionRequest {
                model_id: "fixture-route".to_owned(),
                ..Default::default()
            })
        };
        let mut chat_only = local_target(9);
        chat_only.binding = crate::inference::EngineBinding::new(
            chat_only.binding_identity.clone(),
            std::sync::Arc::clone(chat_only.binding.probe()),
            crate::model::engine::loopback_http::LoopbackInvoker::bind(
                9,
                "fixture-model".to_owned(),
                &[dotcore_proto::dotcore::catalog::v1::InferenceRequestKind::Chat],
                Default::default(),
            ),
        );
        let chat_only_engine = invoke_local_binding(chat_only, decision(), uncancelled());
        let upstream = super::invoke(upstream_target(9, false), decision(), uncancelled(), None);

        for attempt in [chat_only_engine.await, upstream.await] {
            assert!(attempt.allows_fallback());
            let Err(error) = attempt.into_result() else {
                panic!("an unserved request kind must be refused");
            };
            assert!(matches!(error, AdapterError::PreDispatchUnavailable(_)));
            assert_eq!(failure_stage(&error), "unsupported_request_kind");
        }
    }

    #[test]
    fn a_binding_invalid_request_claiming_fallback_is_non_retryable() {
        let attempt = adapter_attempt(
            InferenceAttempt::failed(
                InferenceAttemptDisposition::SafeToFallback,
                InferenceError::InvalidRequest(InvalidRequest::invalid_field(Some("top_k"))),
            ),
            adapter_response,
        );

        assert!(!attempt.allows_fallback());
        assert!(matches!(
            attempt,
            AdapterAttempt::Failed {
                disposition: AdapterAttemptDisposition::NonRetryable,
                error: AdapterError::InvalidRequest(error),
            } if error == InvalidRequest::invalid_field(Some("top_k"))
        ));
    }

    #[test]
    fn an_invalid_request_attempt_never_allows_fallback() {
        let attempt = AdapterAttempt::failed(
            AdapterAttemptDisposition::NonRetryable,
            AdapterError::InvalidRequest(InvalidRequest::invalid_field(Some("top_k"))),
        );

        assert!(!attempt.allows_fallback());
        assert!(matches!(
            attempt.into_result(),
            Err(AdapterError::InvalidRequest(error))
                if error == InvalidRequest::invalid_field(Some("top_k"))
        ));
    }

    async fn collect_events(
        mut events: mpsc::Receiver<Result<ChatStreamEvent, AdapterError>>,
    ) -> Vec<ChatStreamEvent> {
        let mut collected = Vec::new();
        while let Some(event) = timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("adapter stream event timed out")
        {
            collected.push(event.expect("adapter stream event"));
        }
        collected
    }

    /// Dispatches a chat request, the only kind upstream adapter tests send.
    async fn invoke(
        target: SelectedInferenceTarget,
        request: ChatRequest,
        cancellation: RequestCancellation,
        peers: Option<&peer::PeerInferenceClient>,
    ) -> AdapterAttempt {
        super::invoke(target, InferenceRequest::Chat(request), cancellation, peers).await
    }

    fn decode_chat_request(value: &Value) -> ChatRequest {
        let body = json_bytes(value);
        let OpenAiRequest::Chat(request) =
            openai::decode("POST", "/v1/chat/completions", &body).expect("Chat request")
        else {
            panic!("fixture must decode as Chat")
        };
        request
    }

    fn json_bytes(value: &Value) -> Vec<u8> {
        serde_json::to_vec(value).expect("fixture JSON")
    }

    async fn invoke_docker(
        request: ChatRequest,
        response_body: Vec<u8>,
        content_type: &str,
    ) -> (AdapterResponse, Vec<u8>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let content_type = content_type.to_owned();
        let response_length = response_body.len();
        let (request_sender, request_receiver) = tokio::sync::oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let request_body = read_http_body(&mut stream).await;
            request_sender
                .send(request_body)
                .expect("request body receiver");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {response_length}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .await
                .expect("upstream headers");
            stream
                .write_all(&response_body)
                .await
                .expect("upstream body");
        });
        let response = invoke_local_binding(
            local_target(port),
            InferenceRequest::Chat(request),
            uncancelled(),
        )
        .await
        .into_result()
        .expect("llama.cpp adapter");
        let request_body = request_receiver.await.expect("captured request");
        upstream.await.expect("upstream task");
        (response, request_body)
    }

    fn uncancelled() -> RequestCancellation {
        let (_client, client) = watch::channel::<Option<RequestCancellationReason>>(None);
        let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
        RequestCancellation::new(client, route)
    }

    fn cancelled() -> RequestCancellation {
        let (client, client_rx) = watch::channel(None);
        client.send_replace(Some(RequestCancellationReason::ClientDisconnected));
        let (_route, route) = watch::channel::<Option<ServingTargetsRevocationReason>>(None);
        RequestCancellation::new(client_rx, route)
    }

    #[derive(Debug)]
    struct CapturedHttpRequest {
        method_path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl CapturedHttpRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(actual, _)| actual.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    async fn read_http_request(stream: &mut TcpStream) -> CapturedHttpRequest {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).await.expect("upstream request");
            assert!(read > 0, "upstream request ended early");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let header = std::str::from_utf8(&bytes[..header_end]).expect("upstream headers");
                let content_length = header
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                    })
                    .expect("content length");
                break (header_end, content_length);
            }
        };
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let read = stream
                .read(&mut buffer)
                .await
                .expect("upstream request body");
            assert!(read > 0, "upstream request body ended early");
            bytes.extend_from_slice(&buffer[..read]);
        }
        let header = std::str::from_utf8(&bytes[..header_end]).expect("upstream headers");
        let mut lines = header.lines();
        let method_path = lines.next().expect("request line").to_owned();
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(':').expect("header name and value");
                (name.to_owned(), value.trim().to_owned())
            })
            .collect();
        CapturedHttpRequest {
            method_path,
            headers,
            body: bytes[body_start..body_start + content_length].to_vec(),
        }
    }

    async fn read_http_body(stream: &mut TcpStream) -> Vec<u8> {
        read_http_request(stream).await.body
    }

    /// Serves one scripted upstream response and returns the request it received.
    async fn serve_upstream_once(
        status: u16,
        content_type: &'static str,
        body: Vec<u8>,
    ) -> (u16, tokio::task::JoinHandle<CapturedHttpRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("upstream listener");
        let port = listener.local_addr().expect("upstream address").port();
        let upstream = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("upstream connection");
            let request = read_http_request(&mut stream).await;
            write_http_response(&mut stream, status, content_type, &body).await;
            request
        });
        (port, upstream)
    }

    async fn write_http_response(
        stream: &mut TcpStream,
        status: u16,
        content_type: &str,
        body: &[u8],
    ) {
        let head = format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(head.as_bytes())
            .await
            .expect("response headers");
        stream.write_all(body).await.expect("response body");
    }

    fn image_request() -> ChatRequest {
        ChatRequest {
            model_id: "fixture-model".to_owned(),
            messages: vec![ChatMessage {
                role: ChatMessageRole::User as i32,
                content_parts: vec![ChatContentPart {
                    content: Some(chat_content_part::Content::ImageContent(ChatImageContent {
                        media_type: ChatImageMediaType::Png as i32,
                        encoded_data: vec![1, 2],
                    })),
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Returns the request oracle as sent to an endpoint that serves the model as `wire_model`.
    fn oracle_for_model(value: &Value, wire_model: &str) -> Value {
        let mut value = value.clone();
        value
            .as_object_mut()
            .expect("OpenAI-compatible request")
            .insert("model".to_owned(), Value::String(wire_model.to_owned()));
        value
    }

    fn upstream_target(port: u16, supports_image: bool) -> SelectedInferenceTarget {
        let mut target =
            crate::model::controller::serving_targets::upstream_target_for_test_with_dispatch(
                "upstream-fixture",
                &format!("http://127.0.0.1:{port}/v1"),
                "private-model",
                "test-secret",
            );
        target.supports_image = supports_image;
        SelectedInferenceTarget::Upstream(target)
    }

    fn cancellation_with_senders() -> (
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

    fn local_target(port: u16) -> LocalServingTarget {
        let endpoint = LoopbackEndpoint {
            port_name: "http".to_owned(),
            host_port: port,
        };
        LocalServingTarget {
            model_id: CatalogModelId("fixture-model".to_owned()),
            engine_kind: dotcore_proto::dotcore::engine::v1::EngineKind::LlamaCpp as i32,
            deployment_id: ModelDeploymentId("docker-fixture".to_owned()),
            lifecycle_generation: 1,
            reconcile_generation: 1,
            binding_identity: crate::inference::EngineBindingIdentity::new(
                "docker-fixture",
                "fixture-realization",
                "http",
                u64::from(port),
            )
            .expect("binding identity"),
            binding: crate::model::controller::test_support::test_engine_binding_for_values(
                "docker-fixture",
                "fixture-realization",
                "fixture-model",
                u64::from(port),
                Some(endpoint.clone()),
            ),
            image_capacity: None,
            request_kinds: std::collections::BTreeSet::from([
                dotcore_proto::dotcore::catalog::v1::InferenceRequestKind::Chat,
            ]),
        }
    }
}
