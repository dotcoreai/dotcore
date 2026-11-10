//! Inference HTTP connection routing and request lifecycle coordination.

use std::collections::BTreeSet;

use tokio::sync::{OwnedSemaphorePermit, watch};
use tracing::{Instrument, Span, debug, field, info, warn};

use super::{
    errors::{ConnectionError, OpenAiBodyReadError},
    request::{path_component, read_openai_request_body, read_openai_request_head},
    response::{
        ResponseWrite, cancellation_answer, write_cancelled_before_headers, write_openai_response,
        write_openai_response_cancellable,
    },
    stream::{StreamWriteOutcome, write_openai_stream, write_responses_stream},
    watchers::RequestWatchers,
};
use crate::{inference::InferenceRequest, router::metrics::RequestOutcome};

pub(crate) async fn handle_inference_connection(
    stream: tokio::net::TcpStream,
    router: crate::router::selection::InferenceRouter,
    _request_permit: OwnedSemaphorePermit,
    deadline: tokio::time::Instant,
) -> Result<(), ConnectionError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut watchers = RequestWatchers::new();
    let mut cancellation = watchers.subscribe();
    watchers.start_timeout(deadline);
    let head = match read_openai_request_head(&mut reader, &mut cancellation).await {
        Ok(head) => head,
        Err(error @ OpenAiBodyReadError::Io(_)) => return Err(ConnectionError::Request(error)),
        Err(OpenAiBodyReadError::Cancelled(reason)) => {
            return watchers
                .finish_write(write_cancelled_before_headers(&mut writer, reason).await);
        }
        Err(OpenAiBodyReadError::TooLarge) => {
            return watchers.finish_write(
                write_openai_response(&mut writer, crate::router::openai::body_too_large()).await,
            );
        }
    };
    let body = match read_openai_request_body(&mut reader, &head, &mut cancellation).await {
        Ok(body) => body,
        Err(error @ OpenAiBodyReadError::Io(_)) => return Err(ConnectionError::Request(error)),
        Err(OpenAiBodyReadError::Cancelled(reason)) => {
            return watchers
                .finish_write(write_cancelled_before_headers(&mut writer, reason).await);
        }
        Err(OpenAiBodyReadError::TooLarge) => {
            return watchers.finish_write(
                write_openai_response(&mut writer, crate::router::openai::body_too_large()).await,
            );
        }
    };
    watchers.start_client(reader);
    let path = path_component(&head.path).to_owned();
    let span = tracing::info_span!("inference_request", operation = "inference_request", method = %head.method, route = %path, route_classification = route_classification(&path), model_id = field::Empty, status = field::Empty, outcome = field::Empty, cancellation_reason = field::Empty, error_kind = field::Empty);
    let response = match span.in_scope(|| crate::router::openai::decode(&head.method, &path, &body))
    {
        Ok(crate::router::openai::OpenAiRequest::Models) => {
            crate::router::openai::models(router.available_models())
        }
        Ok(crate::router::openai::OpenAiRequest::Chat(request)) => {
            return serve_route(
                OpenAiRoute::Chat(ChatRoute::Completions),
                &mut writer,
                &router,
                InferenceRequest::Chat(request),
                cancellation,
                &span,
                &watchers,
            )
            .await;
        }
        Ok(crate::router::openai::OpenAiRequest::Responses(request, namespaces)) => {
            return serve_route(
                OpenAiRoute::Chat(ChatRoute::Responses(&namespaces)),
                &mut writer,
                &router,
                InferenceRequest::Chat(request),
                cancellation,
                &span,
                &watchers,
            )
            .await;
        }
        Ok(crate::router::openai::OpenAiRequest::SystemOne(request)) => {
            return serve_route(
                OpenAiRoute::Decision(DecisionRoute::SystemOne),
                &mut writer,
                &router,
                InferenceRequest::Decision(request),
                cancellation,
                &span,
                &watchers,
            )
            .await;
        }
        Err(response) => {
            log_request_rejected(&span, &response);
            response
        }
    };
    let (_, route) = watch::channel(None);
    let cancellation = crate::router::cancellation::RequestCancellation::new(cancellation, route);
    let (result, _) = write_response(&mut writer, response, None, cancellation, &span).await;
    watchers.finish_write(result)
}

async fn serve_route(
    route: OpenAiRoute<'_>,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    router: &crate::router::selection::InferenceRouter,
    request: InferenceRequest,
    cancellation: watch::Receiver<Option<crate::router::cancellation::RequestCancellationReason>>,
    span: &Span,
    watchers: &RequestWatchers,
) -> Result<(), ConnectionError> {
    span.record("model_id", request.model_id());
    // Engines always report streamed usage; only a client that asked for it receives it.
    let include_usage = match &request {
        InferenceRequest::Chat(request) => request.include_stream_usage.unwrap_or(false),
        InferenceRequest::Decision(_) => false,
    };
    let mut meter = router.meter(request.kind());
    let admission = span.in_scope(|| {
        let (routes, route) = router.resolve_route(&request)?;
        meter.resolved(&route.inference_route_id);
        router.admit_route(routes, route, &request)
    });
    let mut lease = match admission {
        Ok(lease) => lease,
        Err(error) => {
            warn!(
                parent: span,
                event_kind = "inference_admission_rejected",
                route = %request.model_id(),
                error_kind = selection_error_kind(&error),
                "inference request admission rejected",
            );
            let result = write_openai_response(writer, selection_response(error)).await;
            meter.finish(RequestOutcome::Rejected);
            return watchers.finish_write(result);
        }
    };
    meter.admitted();
    let mut attempted_deployments = BTreeSet::new();
    let (response, response_cancellation, known_cancellation) = loop {
        let route_snapshot = lease.route_snapshot();
        let admitted_route = lease.route();
        let admitted_route_name = admitted_route.name.clone();
        let fallback_group_index = lease.fallback_group_index();
        let attempt_index = attempted_deployments.len();
        let deployment_id = lease.deployment_id().to_owned();
        attempted_deployments.insert(deployment_id.clone());
        log_target_attempt(
            span,
            &admitted_route_name,
            &deployment_id,
            fallback_group_index,
            attempt_index,
        );
        meter.start_attempt(lease.target());
        let (target, target_cancellation) = lease.into_request(cancellation.clone());
        let attempt = crate::router::adapters::invoke(
            target,
            request.clone(),
            target_cancellation.clone(),
            router.peer_client(),
        )
        .instrument(span.clone())
        .await;
        // A decision answer is encoded once, for both its attempt outcome and its response.
        let decision = match &attempt {
            crate::router::adapters::AdapterAttempt::Response(
                crate::router::adapters::AdapterResponse::Decision(result),
            ) => Some(decision_response(
                route,
                &request,
                result,
                &admitted_route_name,
            )),
            _ => None,
        };
        end_answered_attempt(&mut meter, route, decision.as_ref(), &attempt);
        if attempt.allows_fallback() {
            match span.in_scope(|| {
                router.admit_fallback(
                    route_snapshot,
                    admitted_route,
                    &request,
                    fallback_group_index.saturating_add(1),
                    &attempted_deployments,
                )
            }) {
                Ok(next) => {
                    log_fallback_selected(span, &admitted_route_name, &next);
                    meter.fallback();
                    lease = next;
                    continue;
                }
                Err(error) => {
                    log_fallback_exhausted(span, &admitted_route_name, &error);
                    break (
                        fallback_exhausted_response(error),
                        target_cancellation,
                        None,
                    );
                }
            }
        }
        match attempt.into_result() {
            Ok(crate::router::adapters::AdapterResponse::Complete(result)) => {
                let OpenAiRoute::Chat(chat_route) = route else {
                    log_unexpected_result(
                        span,
                        &admitted_route_name,
                        &deployment_id,
                        attempt_index,
                    );
                    break (
                        super::stream::engine_failure_response("unexpected_result"),
                        target_cancellation,
                        None,
                    );
                };
                if matches!(chat_route, ChatRoute::Responses(_)) && result.choices.len() > 1 {
                    // The single-choice Response refuses this result as an invalid engine answer.
                    warn!(
                        parent: span,
                        event_kind = "inference_completion_failed",
                        route = %admitted_route_name,
                        deployment_id = %deployment_id,
                        attempt_index,
                        error_kind = "upstream",
                        failure_stage = "unexpected_choice",
                        "inference completion failed",
                    );
                }
                break (
                    cancellation_response(target_cancellation.reason())
                        .unwrap_or_else(|| chat_route.completion(result, &admitted_route_name)),
                    target_cancellation,
                    None,
                );
            }
            Ok(crate::router::adapters::AdapterResponse::Stream(events)) => {
                let OpenAiRoute::Chat(chat_route) = route else {
                    log_unexpected_result(
                        span,
                        &admitted_route_name,
                        &deployment_id,
                        attempt_index,
                    );
                    break (
                        super::stream::engine_failure_response("unexpected_result"),
                        target_cancellation,
                        None,
                    );
                };
                let result = chat_route
                    .write_stream(
                        writer,
                        events,
                        target_cancellation.clone(),
                        admitted_route_name.clone(),
                        include_usage,
                        &mut |event| meter.observe(event),
                    )
                    .instrument(span.clone())
                    .await;
                if let Ok(StreamWriteOutcome::Failed {
                    error_kind,
                    failure_stage,
                    committed,
                    ..
                }) = &result
                {
                    warn!(
                        parent: span,
                        event_kind = "inference_stream_failed",
                        route = %admitted_route_name,
                        deployment_id = %deployment_id,
                        attempt_index,
                        error_kind,
                        failure_stage,
                        response_committed = committed,
                        "inference stream failed",
                    );
                }
                let (attempt_outcome, stage) = stream_attempt_end(&result);
                meter.end_attempt(attempt_outcome, stage);
                meter.finish(record_stream(span, &result));
                return watchers.finish_write(result.map(|_| ()));
            }
            Ok(crate::router::adapters::AdapterResponse::Decision(_)) => {
                let response = match decision.unwrap_or(Err("unexpected_result")) {
                    Ok(response) => response,
                    Err(failure_stage) => {
                        warn!(
                            parent: span,
                            event_kind = "inference_completion_failed",
                            route = %admitted_route_name,
                            deployment_id = %deployment_id,
                            attempt_index,
                            error_kind = "upstream",
                            failure_stage,
                            "inference completion failed",
                        );
                        super::stream::engine_failure_response(failure_stage)
                    }
                };
                break (
                    cancellation_response(target_cancellation.reason()).unwrap_or(response),
                    target_cancellation,
                    None,
                );
            }
            Ok(crate::router::adapters::AdapterResponse::Upstream(response)) => {
                break (
                    cancellation_response(target_cancellation.reason()).unwrap_or_else(|| {
                        crate::router::openai::upstream(
                            response.status,
                            response.headers,
                            response.body,
                        )
                    }),
                    target_cancellation,
                    None,
                );
            }
            // A node attempt reports its own cancellation, which the local state may not hold.
            Err(crate::router::adapters::AdapterError::Cancelled(reason)) => {
                break (
                    cancellation_answer(reason),
                    target_cancellation,
                    Some(reason),
                );
            }
            Err(
                error @ (crate::router::adapters::AdapterError::Upstream(_)
                | crate::router::adapters::AdapterError::PreDispatchUnavailable(_)),
            ) => {
                break (
                    super::stream::engine_failure_response(crate::router::adapters::failure_stage(
                        &error,
                    )),
                    target_cancellation,
                    None,
                );
            }
            Err(crate::router::adapters::AdapterError::UnsupportedCapability) => {
                break (
                    crate::router::openai::image_unsupported_capability(),
                    target_cancellation,
                    None,
                );
            }
            Err(crate::router::adapters::AdapterError::InvalidRequest(error)) => {
                // Responses clients only recognize a context refusal from a streamed failure.
                let streamed_responses = matches!(
                    route,
                    OpenAiRoute::Chat(ChatRoute::Responses(_))
                ) && matches!(&request, InferenceRequest::Chat(chat) if chat.stream == Some(true));
                break (
                    if streamed_responses
                        && error.category == crate::inference::InvalidRequestCategory::ContextLength
                    {
                        crate::router::openai::responses::context_length_stream_response()
                    } else {
                        crate::router::openai::request_error(error)
                    },
                    target_cancellation,
                    None,
                );
            }
        }
    };
    let (result, outcome) = write_response(
        writer,
        response,
        known_cancellation,
        response_cancellation,
        span,
    )
    .await;
    meter.finish(outcome);
    watchers.finish_write(result)
}

/// Records the end of an attempt that answered without a stream, with the usage of a complete
/// answer; a streamed attempt ends when its stream does.
fn end_answered_attempt(
    meter: &mut crate::router::metrics::RequestMeter,
    route: OpenAiRoute<'_>,
    decision: Option<&DecisionEncoding>,
    attempt: &crate::router::adapters::AdapterAttempt,
) {
    use crate::router::adapters::{AdapterAttempt, AdapterResponse};

    let Some((outcome, stage)) = answered_attempt_end(route, decision, attempt) else {
        return;
    };
    match attempt {
        AdapterAttempt::Response(AdapterResponse::Complete(result)) => {
            if let Some(usage) = &result.usage {
                meter.observe_usage(usage);
            }
        }
        AdapterAttempt::Response(AdapterResponse::Decision(result)) => {
            if let Some(usage) = &result.usage {
                meter.observe_decision_usage(usage);
            }
        }
        _ => {}
    }
    meter.end_attempt(outcome, stage);
}

/// How an attempt that answered without a stream ended for its deployment, with the failure stage
/// of an unsuccessful one; `None` for a stream, which ends with its write. A complete answer the
/// route refuses fails like the same answer streamed, and so does an answer of another kind. A
/// decision answer ends as its `decision` encoding did.
pub(super) fn answered_attempt_end(
    route: OpenAiRoute<'_>,
    decision: Option<&DecisionEncoding>,
    attempt: &crate::router::adapters::AdapterAttempt,
) -> Option<(RequestOutcome, Option<&'static str>)> {
    use crate::router::adapters::{AdapterAttempt, AdapterError, AdapterResponse, failure_stage};

    Some(match attempt {
        AdapterAttempt::Response(AdapterResponse::Stream(_) | AdapterResponse::Complete(_))
            if matches!(route, OpenAiRoute::Decision(_)) =>
        {
            (RequestOutcome::Failed, Some("unexpected_result"))
        }
        AdapterAttempt::Response(AdapterResponse::Stream(_)) => return None,
        AdapterAttempt::Response(AdapterResponse::Complete(result))
            if matches!(route, OpenAiRoute::Chat(ChatRoute::Responses(_)))
                && result.choices.len() > 1 =>
        {
            (RequestOutcome::Failed, Some("unexpected_choice"))
        }
        AdapterAttempt::Response(AdapterResponse::Complete(_)) => (RequestOutcome::Success, None),
        AdapterAttempt::Response(AdapterResponse::Decision(_)) => match decision {
            Some(Ok(_)) => (RequestOutcome::Success, None),
            Some(Err(stage)) => (RequestOutcome::Failed, Some(stage)),
            None => (RequestOutcome::Failed, Some("unexpected_result")),
        },
        AdapterAttempt::Response(AdapterResponse::Upstream(_)) => {
            (RequestOutcome::Failed, Some("upstream_status"))
        }
        AdapterAttempt::Failed {
            error: AdapterError::Cancelled(_),
            ..
        } => (RequestOutcome::Cancelled, None),
        AdapterAttempt::Failed {
            error: error @ (AdapterError::InvalidRequest(_) | AdapterError::UnsupportedCapability),
            ..
        } => (RequestOutcome::Rejected, Some(failure_stage(error))),
        AdapterAttempt::Failed { error, .. } => {
            (RequestOutcome::Failed, Some(failure_stage(error)))
        }
    })
}

/// Writes a decided response and records its outcome on the request span.
///
/// `known` is a cancellation the response already answers that the local state may not hold,
/// such as one a node reported for its attempt.
pub(super) async fn write_response(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    response: crate::router::openai::OpenAiResponse,
    known: Option<crate::router::cancellation::RequestCancellationReason>,
    cancellation: crate::router::cancellation::RequestCancellation,
    span: &Span,
) -> (Result<(), super::errors::OpenAiWriteError>, RequestOutcome) {
    let status = response.status;
    let decided = known.or(cancellation.reason());
    let result = write_openai_response_cancellable(writer, response, cancellation.clone())
        .instrument(span.clone())
        .await;
    let outcome = record_written_response(span, status, decided, &result, &cancellation);
    (result.map(|_| ()), outcome)
}

/// Records how a response write of `status` ended on the request span and returns the outcome.
///
/// The request counts as cancelled when a cancellation was `decided` before the write or
/// interrupted it; a client closing its connection after receiving the whole response, as
/// clients do once they read it, does not count. A failed write is a failure whatever its
/// status, unless the request was cancelled.
pub(super) fn record_written_response(
    span: &Span,
    status: u16,
    decided: Option<crate::router::cancellation::RequestCancellationReason>,
    result: &Result<ResponseWrite, super::errors::OpenAiWriteError>,
    cancellation: &crate::router::cancellation::RequestCancellation,
) -> RequestOutcome {
    match result {
        Ok(ResponseWrite::Complete) => record_response(span, status, decided),
        Ok(ResponseWrite::Replaced { reason, status }) => {
            record_response(span, *status, Some(decided.unwrap_or(*reason)))
        }
        Ok(ResponseWrite::Interrupted(reason)) => {
            record_response(span, status, Some(decided.unwrap_or(*reason)))
        }
        // The response is incomplete, so a cancellation observed since the write counts.
        Err(_) => match decided.or(cancellation.reason()) {
            Some(reason) => record_response(span, status, Some(reason)),
            None => {
                span.record("status", status);
                span.record("outcome", RequestOutcome::Failed.as_str());
                span.record("error_kind", "response_write");
                RequestOutcome::Failed
            }
        },
    }
}

/// Public inference API route a request arrived on, which decides how its answer is encoded.
#[derive(Clone, Copy)]
pub(super) enum OpenAiRoute<'a> {
    /// A chat route, answering complete or streamed chat results.
    Chat(ChatRoute<'a>),

    /// A decision route, answering decision results.
    Decision(DecisionRoute),
}

/// Public decision route encoding decision results.
#[derive(Clone, Copy)]
pub(super) enum DecisionRoute {
    SystemOne,
}

/// Public chat route encoding chat results.
#[derive(Clone, Copy)]
pub(super) enum ChatRoute<'a> {
    Completions,

    /// The Responses route, with the request's namespaced tools its function calls go back under.
    Responses(&'a crate::router::openai::ToolNamespaces),
}

impl ChatRoute<'_> {
    fn completion(
        self,
        result: dotcore_proto::dotcore::inference::v1::ChatCompletionResult,
        admitted_route_name: &str,
    ) -> crate::router::openai::OpenAiResponse {
        match self {
            Self::Completions => crate::router::openai::completion(result, admitted_route_name),
            Self::Responses(namespaces) => crate::router::openai::responses::completion(
                result,
                admitted_route_name,
                namespaces,
            ),
        }
    }

    async fn write_stream(
        self,
        writer: &mut tokio::net::tcp::OwnedWriteHalf,
        events: tokio::sync::mpsc::Receiver<
            Result<
                dotcore_proto::dotcore::inference::v1::ChatStreamEvent,
                crate::router::adapters::AdapterError,
            >,
        >,
        cancellation: crate::router::cancellation::RequestCancellation,
        admitted_route_name: String,
        include_usage: bool,
        observe: &mut (dyn FnMut(&dotcore_proto::dotcore::inference::v1::ChatStreamEvent) + Send),
    ) -> Result<StreamWriteOutcome, super::errors::OpenAiWriteError> {
        match self {
            Self::Completions => {
                write_openai_stream(
                    writer,
                    events,
                    cancellation,
                    admitted_route_name,
                    include_usage,
                    observe,
                )
                .await
            }
            Self::Responses(namespaces) => {
                write_responses_stream(
                    writer,
                    events,
                    cancellation,
                    admitted_route_name,
                    namespaces,
                    observe,
                )
                .await
            }
        }
    }
}

/// A decision answer's public response, or the failure stage when its route cannot answer it.
pub(super) type DecisionEncoding = Result<crate::router::openai::OpenAiResponse, &'static str>;

/// Encodes a decision result for the route, or names the failure stage when the route cannot
/// answer it: only decision routes answer decisions, and only results fitting the request's
/// questions.
fn decision_response(
    route: OpenAiRoute,
    request: &InferenceRequest,
    result: &dotcore_proto::dotcore::inference::v1::DecisionResult,
    admitted_route_name: &str,
) -> DecisionEncoding {
    match (route, request) {
        (OpenAiRoute::Decision(DecisionRoute::SystemOne), InferenceRequest::Decision(request)) => {
            crate::router::systemone::encode_result(request, result, admitted_route_name)
                .map(|body| crate::router::openai::OpenAiResponse {
                    status: 200,
                    content_type: "application/json".to_owned(),
                    body,
                    passthrough_headers: Vec::new(),
                })
                .map_err(|error| error.stage())
        }
        _ => Err("unexpected_result"),
    }
}

/// Logs a public request refused before it reached a route, with the error the client receives.
///
/// The error names only recognized fields and fixed messages, never request values, so it is
/// safe to log whole.
fn log_request_rejected(span: &Span, response: &crate::router::openai::OpenAiResponse) {
    let body = serde_json::from_slice::<serde_json::Value>(&response.body).ok();
    let error_field = |name: &str| {
        body.as_ref()
            .and_then(|body| body.get("error")?.get(name)?.as_str())
            .unwrap_or("none")
            .to_owned()
    };
    info!(
        parent: span,
        event_kind = "inference_request_rejected",
        status = response.status,
        error_type = error_field("type"),
        code = error_field("code"),
        param = error_field("param"),
        error_message = error_field("message"),
        "inference request rejected",
    );
}

fn log_unexpected_result(span: &Span, route: &str, deployment_id: &str, attempt_index: usize) {
    warn!(
        parent: span,
        event_kind = "inference_completion_failed",
        route = %route,
        deployment_id = %deployment_id,
        attempt_index,
        error_kind = "upstream",
        failure_stage = "unexpected_result",
        "inference completion failed",
    );
}

pub(super) fn log_target_attempt(
    span: &Span,
    route: &str,
    deployment_id: &str,
    fallback_group_index: usize,
    attempt_index: usize,
) {
    debug!(
        parent: span,
        event_kind = "inference_target_attempt",
        route = %route,
        deployment_id = %deployment_id,
        fallback_group_index,
        attempt_index,
        "inference target attempt",
    );
}

fn log_fallback_selected(
    span: &Span,
    route: &str,
    next: &crate::router::selection::InferenceTargetLease,
) {
    debug!(
        parent: span,
        event_kind = "inference_fallback_selected",
        route = %route,
        deployment_id = next.deployment_id(),
        fallback_group_index = next.fallback_group_index(),
        "inference fallback target selected",
    );
}

pub(super) fn log_fallback_exhausted(
    span: &Span,
    route: &str,
    error: &crate::router::selection::TargetSelectionError,
) {
    warn!(
        parent: span,
        event_kind = "inference_fallback_exhausted",
        route = %route,
        error_kind = selection_error_kind(error),
        "inference fallback exhausted",
    );
}

pub(super) fn selection_response(
    error: crate::router::selection::TargetSelectionError,
) -> crate::router::openai::OpenAiResponse {
    match error {
        crate::router::selection::TargetSelectionError::NotFound => {
            crate::router::openai::route_not_found()
        }
        crate::router::selection::TargetSelectionError::Unavailable => {
            crate::router::openai::no_ready_target()
        }
        crate::router::selection::TargetSelectionError::InvalidImage(cause) => {
            crate::router::openai::image_error(&cause)
        }
        crate::router::selection::TargetSelectionError::InvalidRequest(error) => {
            crate::router::openai::request_error(error)
        }
        crate::router::selection::TargetSelectionError::RequestKindNotServed => {
            crate::router::openai::request_kind_not_served()
        }
    }
}

/// Answers a route whose ready targets all refused before dispatch: the route exists and had
/// ready members, so it is temporarily unavailable rather than without ready targets.
pub(super) fn fallback_exhausted_response(
    error: crate::router::selection::TargetSelectionError,
) -> crate::router::openai::OpenAiResponse {
    match error {
        crate::router::selection::TargetSelectionError::Unavailable => {
            crate::router::openai::route_unavailable()
        }
        error => selection_response(error),
    }
}

fn selection_error_kind(error: &crate::router::selection::TargetSelectionError) -> &'static str {
    match error {
        crate::router::selection::TargetSelectionError::NotFound => "route_not_found",
        crate::router::selection::TargetSelectionError::Unavailable => "target_unavailable",
        crate::router::selection::TargetSelectionError::InvalidImage(_) => "invalid_image",
        crate::router::selection::TargetSelectionError::InvalidRequest(_) => "invalid_request",
        crate::router::selection::TargetSelectionError::RequestKindNotServed => {
            "request_kind_not_served"
        }
    }
}

fn cancellation_response(
    reason: Option<crate::router::cancellation::RequestCancellationReason>,
) -> Option<crate::router::openai::OpenAiResponse> {
    reason.map(cancellation_answer)
}

fn route_classification(path: &str) -> &'static str {
    if matches!(
        path,
        "/v1/models"
            | "/v1/chat/completions"
            | "/v1/responses"
            | "/v1/systemone"
            | "/v1/embeddings"
    ) {
        "known"
    } else {
        "unknown"
    }
}

/// Records a written response's status and outcome on the request span and returns the outcome.
fn record_response(
    span: &Span,
    status: u16,
    reason: Option<crate::router::cancellation::RequestCancellationReason>,
) -> RequestOutcome {
    let outcome = if reason.is_some() {
        RequestOutcome::Cancelled
    } else if (200..300).contains(&status) {
        RequestOutcome::Success
    } else {
        RequestOutcome::Failed
    };
    span.record("status", status);
    span.record("outcome", outcome.as_str());
    if let Some(reason) = reason {
        span.record("cancellation_reason", format!("{reason:?}"));
    }
    outcome
}

/// Records a written stream's status and outcome on the request span and returns the outcome.
/// A complete stream succeeded even when the client closed its connection right after it.
pub(super) fn record_stream(
    span: &Span,
    result: &Result<StreamWriteOutcome, super::errors::OpenAiWriteError>,
) -> RequestOutcome {
    match result {
        Ok(StreamWriteOutcome::Complete) => record_response(span, 200, None),
        Ok(StreamWriteOutcome::Cancelled(reason)) => record_response(span, 499, Some(*reason)),
        Ok(StreamWriteOutcome::Failed {
            error_kind, status, ..
        }) => {
            span.record("status", status);
            span.record("outcome", RequestOutcome::Failed.as_str());
            span.record("error_kind", error_kind);
            RequestOutcome::Failed
        }
        Err(_) => {
            span.record("status", 500);
            span.record("outcome", RequestOutcome::Failed.as_str());
            span.record("error_kind", "stream_write");
            RequestOutcome::Failed
        }
    }
}

/// How a streamed attempt ended for its deployment, with the failure stage of an unsuccessful one.
/// A client write that failed is the client's connection, not the deployment, so it counts as
/// cancelled; a frame that could not be encoded fails the attempt. A request the target refused
/// is rejected, as when it answers without a stream.
pub(super) fn stream_attempt_end(
    result: &Result<StreamWriteOutcome, super::errors::OpenAiWriteError>,
) -> (RequestOutcome, Option<&'static str>) {
    match result {
        Ok(StreamWriteOutcome::Complete) => (RequestOutcome::Success, None),
        Ok(StreamWriteOutcome::Cancelled(_)) | Err(super::errors::OpenAiWriteError::Io(_)) => {
            (RequestOutcome::Cancelled, None)
        }
        Ok(StreamWriteOutcome::Failed {
            error_kind: "invalid_request" | "unsupported_capability",
            failure_stage,
            ..
        }) => (RequestOutcome::Rejected, Some(failure_stage)),
        Ok(StreamWriteOutcome::Failed { failure_stage, .. }) => {
            (RequestOutcome::Failed, Some(failure_stage))
        }
        Err(super::errors::OpenAiWriteError::Encoding(_)) => {
            (RequestOutcome::Failed, Some("stream_encoding"))
        }
    }
}
