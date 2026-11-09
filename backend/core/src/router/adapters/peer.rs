//! Forwards a request the manager router already admitted to the one node realization that
//! serves it and returns the node's completion, stream, or passthrough answer unchanged. A node in
//! another process is reached over the pinned private channel to that node; the node sharing the
//! manager process is reached in process, through the same exact-target admission its private
//! service runs, without protocol encoding or TLS.
//!
//! Only an unreachable or unverifiable node, a node availability refusal, or a node engine
//! refusal before any output may fall back to another target; anything after the node admits
//! the request is started or ambiguous. An invalid request never falls back, and an unknown
//! refusal fails closed. Ending the call, on client disconnect or lease revocation, cancels the
//! node's attempt.

use std::{error::Error as _, sync::Arc};

use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;
use dotcore_proto::dotcore::inference::{
    private::v1::{
        ExactServingTarget, ExactTargetFailure, ExactTargetFailureDisposition,
        ExactTargetFailureKind, ExactTargetRejected, ExactTargetRejectionReason,
        InvokeExactTargetRequest, InvokeExactTargetResponse, PassthroughResponse,
        invoke_exact_target_request, invoke_exact_target_response::Frame,
        peer_inference_service_client::PeerInferenceServiceClient,
    },
    v1::ChatStreamEvent,
};
use dotcore_site::worker_channels::{WorkerAddress, WorkerChannelPurpose, WorkerChannels};
use tokio::sync::{mpsc, watch};
use tonic::{Code, Status, Streaming};
use tracing::warn;

use super::{
    AdapterAttempt, AdapterAttemptDisposition, AdapterError, AdapterResponse, UpstreamResponse,
};
use crate::{
    inference::{InferenceError, InferenceRequest},
    model::controller::serving_targets::{NodeServingTarget, NodeTransport},
    router::{
        cancellation::{RequestCancellation, RequestCancellationReason},
        peer_service::{
            ExactTargetAdmission, PEER_INFERENCE_MESSAGE_LIMIT_BYTES, peer_invalid_request,
        },
    },
};

/// Stream events buffered toward the public response writer before the worker waits for it.
const EVENT_BUFFER: usize = 8;

/// Manager outbound client for node-hosted exact-target inference.
///
/// It dials other nodes only through the manager's shared pinned worker channels, so it accepts
/// only the exact pinned certificate of the SiteNode the target names, and admits targets of the
/// node sharing its process through that node's own exact-target admission.
#[derive(Clone)]
pub(crate) struct PeerInferenceClient {
    /// Channels to other nodes; absent on a manager without a private listener.
    channels: Option<WorkerChannels>,

    /// Exact-target admission of the node sharing the manager process.
    local: Arc<ExactTargetAdmission>,
}

impl PeerInferenceClient {
    pub(crate) fn new(channels: Option<WorkerChannels>, local: ExactTargetAdmission) -> Self {
        Self {
            channels,
            local: Arc::new(local),
        }
    }
}

/// Makes one exact-target attempt on the node hosting `target`, never retrying it.
pub(crate) async fn invoke(
    client: &PeerInferenceClient,
    target: NodeServingTarget,
    request: InferenceRequest,
    cancellation: RequestCancellation,
) -> AdapterAttempt {
    match target.dispatch().transport.clone() {
        NodeTransport::InProcess => {
            invoke_in_process(&client.local, &target, request, cancellation).await
        }
        NodeTransport::Remote { address } => match &client.channels {
            Some(channels) => invoke_remote(channels, address, target, request, cancellation).await,
            None => refused(
                &target,
                "private_transport_unconfigured",
                anyhow::anyhow!("the manager has no private listener to reach other nodes"),
            ),
        },
    }
}

/// Admits `target` on the node sharing the manager process and makes its one engine attempt.
///
/// The attempt ends on the router's own cancellation, client disconnect or route revocation, and
/// on revocation of the node's local binding, exactly as a remote node's attempt does.
async fn invoke_in_process(
    admission: &ExactTargetAdmission,
    target: &NodeServingTarget,
    request: InferenceRequest,
    mut cancellation: RequestCancellation,
) -> AdapterAttempt {
    let (local, lease) = match admission.admit(&exact_target(target)) {
        Ok(admitted) => admitted,
        Err(reason) => {
            return refused(
                target,
                reason.as_str_name(),
                anyhow::anyhow!("node rejected the exact target before dispatch"),
            );
        }
    };
    let (_, binding) = lease.into_parts();
    let (forward, router) = watch::channel(None);
    // Carries the router's cancellation into the node attempt for as long as it may observe it;
    // the bridge ends once the attempt drops every receiver.
    tokio::spawn(async move {
        tokio::select! {
            reason = cancellation.cancelled() => {
                forward.send_replace(Some(reason));
            }
            () = forward.closed() => {}
        }
    });
    super::invoke_local_binding(local, request, RequestCancellation::new(router, binding)).await
}

/// Makes one exact-target attempt on another node over its private channel at `address`.
async fn invoke_remote(
    channels: &WorkerChannels,
    address: WorkerAddress,
    target: NodeServingTarget,
    request: InferenceRequest,
    mut cancellation: RequestCancellation,
) -> AdapterAttempt {
    let channel = match channels.channel(
        &target.site_node_id,
        WorkerChannelPurpose::Inference,
        address,
    ) {
        Ok(channel) => channel,
        Err(error) => {
            return refused(
                &target,
                "channel_unavailable",
                anyhow::Error::new(error).context("worker channel is unavailable"),
            );
        }
    };
    let kind = request.kind();
    let request = InvokeExactTargetRequest {
        target: Some(exact_target(&target)),
        request: Some(match request {
            InferenceRequest::Chat(request) => invoke_exact_target_request::Request::Chat(request),
            InferenceRequest::Decision(request) => {
                invoke_exact_target_request::Request::Decision(request)
            }
        }),
    };
    let mut peer = PeerInferenceServiceClient::new(channel)
        .max_decoding_message_size(PEER_INFERENCE_MESSAGE_LIMIT_BYTES)
        .max_encoding_message_size(PEER_INFERENCE_MESSAGE_LIMIT_BYTES);
    let call = tokio::select! {
        biased;
        reason = cancellation.cancelled() => return cancelled(reason),
        call = peer.invoke_exact_target(request) => call,
    };
    let mut frames = match call {
        Ok(response) => response.into_inner(),
        Err(status) => return call_failed(&target, status),
    };

    match next_frame(&mut frames, &mut cancellation).await {
        Ok(Some(Frame::Admitted(_))) => {}
        Ok(Some(Frame::Rejected(rejected))) => return rejected_attempt(&target, rejected),
        Ok(Some(_)) => return started(protocol_violation("worker sent output before admission")),
        Ok(None) => return started(protocol_violation("worker ended the call before admission")),
        Err(error) => return started(error),
    }

    // Exactly one outcome follows admission; a stream carries its remaining events after it. A
    // chat request never yields a decision result, and a decision request never yields chat.
    let chat = kind == InferenceRequestKind::Chat;
    match next_frame(&mut frames, &mut cancellation).await {
        Ok(Some(Frame::Completion(result))) if chat => {
            AdapterAttempt::Response(AdapterResponse::Complete(result))
        }
        Ok(Some(Frame::StreamEvent(event))) if chat => AdapterAttempt::Response(
            AdapterResponse::Stream(forward_stream(event, frames, cancellation)),
        ),
        Ok(Some(Frame::DecisionResult(result))) if !chat => {
            AdapterAttempt::Response(AdapterResponse::Decision(result))
        }
        Ok(Some(Frame::Completion(_) | Frame::StreamEvent(_) | Frame::DecisionResult(_))) => {
            started(protocol_violation(
                "worker answered with a result of another request kind",
            ))
        }
        Ok(Some(Frame::Passthrough(response))) => match passthrough(response) {
            Ok(response) => AdapterAttempt::Response(AdapterResponse::Upstream(response)),
            Err(error) => started(error),
        },
        Ok(Some(Frame::Failure(failure))) => {
            // No output was forwarded yet, so the worker's own disposition is honored.
            AdapterAttempt::failed(failure_disposition(&failure), failure_error(&failure))
        }
        Ok(Some(Frame::Admitted(_) | Frame::Rejected(_))) => {
            started(protocol_violation("worker repeated its admission outcome"))
        }
        Ok(None) => started(protocol_violation(
            "worker ended the call without an outcome",
        )),
        Err(error) => started(error),
    }
}

/// Forwards the remaining stream events until the worker ends the call, fails, or the request
/// is cancelled. Returning drops the call, which cancels the worker's attempt.
fn forward_stream(
    first: ChatStreamEvent,
    mut frames: Streaming<InvokeExactTargetResponse>,
    mut cancellation: RequestCancellation,
) -> mpsc::Receiver<Result<ChatStreamEvent, AdapterError>> {
    let (sender, receiver) = mpsc::channel(EVENT_BUFFER);
    tokio::spawn(async move {
        let mut next = Ok(Some(Frame::StreamEvent(first)));
        loop {
            let item = match next {
                Ok(Some(Frame::StreamEvent(event))) => Ok(event),
                // Output was already forwarded, so any failure is started whatever the worker
                // reports.
                Ok(Some(Frame::Failure(failure))) => Err(failure_error(&failure)),
                Ok(Some(_)) => Err(protocol_violation(
                    "worker sent a non-stream frame inside a stream",
                )),
                Ok(None) => return,
                Err(error) => Err(error),
            };
            let terminal = item.is_err();
            if sender.send(item).await.is_err() || terminal {
                return;
            }
            next = tokio::select! {
                biased;
                () = sender.closed() => return,
                frame = next_frame(&mut frames, &mut cancellation) => frame,
            };
        }
    });
    receiver
}

/// Reads the next frame, or the request's cancellation if it comes first.
async fn next_frame(
    frames: &mut Streaming<InvokeExactTargetResponse>,
    cancellation: &mut RequestCancellation,
) -> Result<Option<Frame>, AdapterError> {
    let message = tokio::select! {
        biased;
        reason = cancellation.cancelled() => return Err(AdapterError::Cancelled(reason)),
        message = frames.message() => message,
    };
    match message {
        Ok(Some(InvokeExactTargetResponse { frame: Some(frame) })) => Ok(Some(frame)),
        Ok(Some(InvokeExactTargetResponse { frame: None })) => {
            Err(protocol_violation("worker sent an empty frame"))
        }
        Ok(None) => Ok(None),
        Err(status) => Err(AdapterError::Upstream(
            anyhow::Error::new(status).context("worker exact-target call was interrupted"),
        )),
    }
}

fn exact_target(target: &NodeServingTarget) -> ExactServingTarget {
    let dispatch = target.dispatch();
    ExactServingTarget {
        manager_session_id: dispatch.manager_session.as_str().to_owned(),
        node_incarnation_id: dispatch.node_incarnation.as_str().to_owned(),
        model_deployment_id: target.deployment_id.0.clone(),
        lifecycle_generation: target.lifecycle_generation,
        binding_token: dispatch.binding_token.clone(),
    }
}

/// Classifies a call that failed before the worker answered with any frame.
///
/// Only a failed connection or TLS handshake, or a refusal the worker issues before any
/// handler runs, proves that no engine work started; any other loss may follow admission.
fn call_failed(target: &NodeServingTarget, status: Status) -> AdapterAttempt {
    let reason = match status.code() {
        Code::Unauthenticated => Some("unauthenticated"),
        Code::PermissionDenied => Some("permission_denied"),
        Code::Unimplemented => Some("unimplemented"),
        _ if connection_failed(&status) => Some("connect_failed"),
        _ => None,
    };
    match reason {
        Some(reason) => refused(
            target,
            reason,
            anyhow::Error::new(status).context("worker refused or was unreachable before dispatch"),
        ),
        None => {
            warn!(
                event_kind = "peer_inference_interrupted",
                deployment_id = %target.deployment_id.0,
                node_id = %target.site_node_id,
                code = ?status.code(),
                "Worker exact-target call failed before any frame",
            );
            started(AdapterError::Upstream(
                anyhow::Error::new(status).context("worker exact-target call failed"),
            ))
        }
    }
}

fn connection_failed(status: &Status) -> bool {
    let mut source = status.source();
    while let Some(error) = source {
        if error.is::<tonic::ConnectError>() {
            return true;
        }
        source = error.source();
    }
    false
}

/// Classifies a worker refusal: availability reasons may fall back, an invalid request never
/// does, and an unknown reason fails closed.
fn rejected_attempt(target: &NodeServingTarget, rejected: ExactTargetRejected) -> AdapterAttempt {
    let reason = ExactTargetRejectionReason::try_from(rejected.reason)
        .unwrap_or(ExactTargetRejectionReason::Unspecified);
    match reason {
        ExactTargetRejectionReason::NodeIncarnationMismatch
        | ExactTargetRejectionReason::ManagerSessionMismatch
        | ExactTargetRejectionReason::DeploymentNotAssigned
        | ExactTargetRejectionReason::BindingUnavailable => refused(
            target,
            reason.as_str_name(),
            anyhow::anyhow!("worker rejected the exact target before dispatch"),
        ),
        ExactTargetRejectionReason::InvalidRequest => {
            let invalid = peer_invalid_request(
                rejected.request_error_category,
                rejected.safe_param.as_deref(),
            );
            warn!(
                event_kind = "peer_inference_refused",
                deployment_id = %target.deployment_id.0,
                node_id = %target.site_node_id,
                reason = reason.as_str_name(),
                param = invalid.param.unwrap_or("unspecified"),
                "Worker refused an invalid exact-target request",
            );
            AdapterAttempt::failed(
                AdapterAttemptDisposition::NonRetryable,
                AdapterError::InvalidRequest(invalid),
            )
        }
        ExactTargetRejectionReason::Unspecified => {
            warn!(
                event_kind = "peer_inference_refused",
                deployment_id = %target.deployment_id.0,
                node_id = %target.site_node_id,
                reason = reason.as_str_name(),
                "Worker refused the exact target for an unknown reason",
            );
            started(protocol_violation(
                "worker rejected the exact target for an unknown reason",
            ))
        }
    }
}

/// Records a refusal that provably preceded any worker engine work, so fallback is safe.
fn refused(
    target: &NodeServingTarget,
    reason: &'static str,
    error: anyhow::Error,
) -> AdapterAttempt {
    warn!(
        event_kind = "peer_inference_refused",
        deployment_id = %target.deployment_id.0,
        node_id = %target.site_node_id,
        reason,
        "Worker exact-target inference refused before dispatch",
    );
    AdapterAttempt::failed(
        AdapterAttemptDisposition::SafeToFallback,
        AdapterError::PreDispatchUnavailable(error),
    )
}

fn passthrough(response: PassthroughResponse) -> Result<UpstreamResponse, AdapterError> {
    let status = u16::try_from(response.status)
        .map_err(|_| protocol_violation("worker passthrough status is out of range"))?;
    Ok(UpstreamResponse {
        status,
        headers: response
            .headers
            .into_iter()
            .map(|header| (header.name, header.value))
            .collect(),
        body: response.body,
    })
}

/// Honors the worker's disposition, except that a request error never falls back.
fn failure_disposition(failure: &ExactTargetFailure) -> AdapterAttemptDisposition {
    if is_request_error(failure) {
        return AdapterAttemptDisposition::NonRetryable;
    }
    match ExactTargetFailureDisposition::try_from(failure.disposition) {
        Ok(ExactTargetFailureDisposition::SafeToFallback) => {
            AdapterAttemptDisposition::SafeToFallback
        }
        Ok(ExactTargetFailureDisposition::NonRetryable) => AdapterAttemptDisposition::NonRetryable,
        _ => AdapterAttemptDisposition::StartedOrAmbiguous,
    }
}

fn is_request_error(failure: &ExactTargetFailure) -> bool {
    failure.kind == ExactTargetFailureKind::InvalidRequest as i32
        || failure.request_error_category.is_some()
}

fn failure_error(failure: &ExactTargetFailure) -> AdapterError {
    if is_request_error(failure) {
        return AdapterError::InvalidRequest(peer_invalid_request(
            failure.request_error_category,
            failure.safe_param.as_deref(),
        ));
    }
    match ExactTargetFailureKind::try_from(failure.kind) {
        Ok(ExactTargetFailureKind::UnsupportedCapability) => AdapterError::UnsupportedCapability,
        Ok(ExactTargetFailureKind::ClientDisconnected) => {
            AdapterError::Cancelled(RequestCancellationReason::ClientDisconnected)
        }
        Ok(ExactTargetFailureKind::RequestTimedOut) => {
            AdapterError::Cancelled(RequestCancellationReason::RequestTimedOut)
        }
        Ok(ExactTargetFailureKind::RouteRevoked) => {
            AdapterError::Cancelled(RequestCancellationReason::RouteRevoked)
        }
        // Reported like a local refusal, so both paths share one failure stage.
        Ok(ExactTargetFailureKind::UnsupportedRequestKind) => AdapterError::PreDispatchUnavailable(
            anyhow::Error::new(InferenceError::UnsupportedRequestKind),
        ),
        Ok(
            ExactTargetFailureKind::Provider
            | ExactTargetFailureKind::Unspecified
            | ExactTargetFailureKind::InvalidRequest,
        )
        | Err(_) => AdapterError::upstream("worker engine attempt failed"),
    }
}

fn cancelled(reason: RequestCancellationReason) -> AdapterAttempt {
    started(AdapterError::Cancelled(reason))
}

fn started(error: AdapterError) -> AdapterAttempt {
    AdapterAttempt::failed(AdapterAttemptDisposition::StartedOrAmbiguous, error)
}

fn protocol_violation(message: &str) -> AdapterError {
    AdapterError::upstream(message)
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use dotcore_proto::dotcore::inference::private::v1::RequestErrorCategory;
    use dotcore_proto::dotcore::inference::v1::{
        ChatContentPart, ChatImageContent, ChatImageMediaType, ChatMessage, ChatMessageRole,
        ChatReasoningDelta, ChatRequest, ChatStreamCompleted, ChatStreamStarted, ChatTextDelta,
        TokenUsage, chat_content_part, chat_stream_event::Event, chat_text_delta,
    };
    use dotcore_site::private_transport::PrivateNodeRole;
    use tokio::{
        net::{TcpListener, TcpStream},
        sync::watch,
        time::timeout,
    };

    use super::*;
    use crate::{
        inference::InvalidRequest,
        model::{
            controller::serving_targets::{
                NodeDispatch, NodeTransport, ServingTargetsRevocationReason,
            },
            domain::ModelDeploymentId,
        },
        router::{
            openai::{self, OpenAiRequest},
            peer_service::test_support::{
                DEPLOYMENT, Fixture, GENERATION, SITE_ID, Script, WORKER_NODE_ID, completion,
                decision_request, decision_result,
            },
            test_fixtures,
        },
        test_support::private_tls::TestAuthority,
    };

    const MANAGER_NODE_ID: &str = "manager-one";

    #[tokio::test]
    async fn forwards_the_canonical_request_and_returns_the_worker_completion() {
        let peer = PeerFixture::start(Script::Complete).await;
        let request = canonical_request();

        let attempt = peer.invoke(request.clone(), uncancelled()).await;

        let AdapterAttempt::Response(AdapterResponse::Complete(result)) = attempt else {
            panic!("the worker completion must be returned");
        };
        assert_eq!(result, completion());
        assert_eq!(peer.worker.invoker.requests(), [request]);
    }

    #[tokio::test]
    async fn forwards_stream_events_in_order_including_reasoning_and_usage() {
        let events = vec![
            Event::Started(ChatStreamStarted {
                completion_id: "completion-a".to_owned(),
                model_id: "route-a".to_owned(),
                created_at_unix_seconds: 7,
            }),
            Event::ReasoningDelta(ChatReasoningDelta {
                choice_index: 0,
                reasoning: "thinking".to_owned(),
            }),
            Event::TextDelta(ChatTextDelta {
                choice_index: 0,
                role: None,
                content: Some(chat_text_delta::Content::TextContent("hello".to_owned())),
            }),
            Event::Usage(TokenUsage {
                prompt_tokens: 3,
                completion_tokens: 2,
                total_tokens: 5,
                cached_prompt_tokens: Some(1),
                reasoning_tokens: Some(2),
            }),
            Event::Completed(ChatStreamCompleted {}),
        ];
        let peer = PeerFixture::start(Script::Stream(events.clone())).await;

        let attempt = peer.invoke(stream_request(), uncancelled()).await;

        let AdapterAttempt::Response(AdapterResponse::Stream(stream)) = attempt else {
            panic!("the worker stream must be returned");
        };
        assert_eq!(
            collect(stream)
                .await
                .into_iter()
                .map(|item| item.expect("forwarded event"))
                .collect::<Vec<_>>(),
            events
                .into_iter()
                .map(|event| ChatStreamEvent { event: Some(event) })
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn forwards_a_passthrough_response_unchanged() {
        let peer = PeerFixture::start(Script::Passthrough).await;

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        let AdapterAttempt::Response(AdapterResponse::Upstream(response)) = attempt else {
            panic!("the worker passthrough must be returned");
        };
        assert_eq!(
            (response.status, response.headers, response.body),
            (
                400,
                vec![("content-type".to_owned(), "application/json".to_owned())],
                b"{\"error\":\"bad\"}".to_vec(),
            )
        );
    }

    #[tokio::test]
    async fn forwards_request_controls_and_extensions_unchanged_to_the_worker() {
        let peer = PeerFixture::start(Script::Complete).await;
        let OpenAiRequest::Chat(mut request) = openai::decode(
            "POST",
            "/v1/chat/completions",
            test_fixtures::PI_CHAT_REQUEST.as_bytes(),
        )
        .expect("Pi request") else {
            panic!("Pi fixture must decode as Chat");
        };
        request.stream = Some(false);
        request.request_extensions_json = Some(test_fixtures::EXACT_NUMBER_EXTENSIONS.to_owned());

        let attempt = peer.invoke(request.clone(), uncancelled()).await;

        assert!(matches!(
            attempt,
            AdapterAttempt::Response(AdapterResponse::Complete(_))
        ));
        let received = peer.worker.invoker.requests();
        assert_eq!(received, [request]);
        assert_eq!(
            received[0].request_extensions_json.as_deref(),
            Some(test_fixtures::EXACT_NUMBER_EXTENSIONS)
        );
        assert_eq!(received[0].store, Some(false));
    }

    #[tokio::test]
    async fn a_worker_invalid_request_refusal_never_falls_back() {
        let peer = PeerFixture::start(Script::Complete).await;
        let request = ChatRequest {
            request_extensions_json: Some(r#"{"model":"other-route"}"#.to_owned()),
            ..canonical_request()
        };

        let attempt = peer.invoke(request, uncancelled()).await;

        assert!(!attempt.allows_fallback());
        assert!(matches!(
            attempt,
            AdapterAttempt::Failed {
                disposition: AdapterAttemptDisposition::NonRetryable,
                error: AdapterError::InvalidRequest(error),
            } if error == InvalidRequest::invalid_field(Some("model"))
        ));
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_worker_request_error_after_admission_never_falls_back() {
        let peer = PeerFixture::start(Script::InvalidRequest(InvalidRequest::invalid_field(Some(
            "top_k",
        ))))
        .await;

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert!(!attempt.allows_fallback());
        assert!(matches!(
            attempt,
            AdapterAttempt::Failed {
                disposition: AdapterAttemptDisposition::NonRetryable,
                error: AdapterError::InvalidRequest(error),
            } if error == InvalidRequest::invalid_field(Some("top_k"))
        ));
        assert_eq!(peer.worker.invoker.requests().len(), 1);
    }

    #[tokio::test]
    async fn request_errors_override_claimed_fallback_and_unknown_refusals_fail_closed() {
        let claimed_fallback = ExactTargetFailure {
            disposition: ExactTargetFailureDisposition::SafeToFallback as i32,
            kind: ExactTargetFailureKind::InvalidRequest as i32,
            safe_param: Some("sk-private-key".to_owned()),
            request_error_category: Some(99),
        };
        assert_eq!(
            failure_disposition(&claimed_fallback),
            AdapterAttemptDisposition::NonRetryable
        );
        assert!(matches!(
            failure_error(&claimed_fallback),
            AdapterError::InvalidRequest(error) if error == InvalidRequest::invalid_field(None)
        ));
        let categorized_provider = ExactTargetFailure {
            disposition: ExactTargetFailureDisposition::SafeToFallback as i32,
            kind: ExactTargetFailureKind::Provider as i32,
            safe_param: Some("n".to_owned()),
            request_error_category: Some(RequestErrorCategory::UnsupportedFeature as i32),
        };
        assert_eq!(
            failure_disposition(&categorized_provider),
            AdapterAttemptDisposition::NonRetryable
        );
        assert!(matches!(
            failure_error(&categorized_provider),
            AdapterError::InvalidRequest(error) if error == InvalidRequest::unsupported_feature(Some("n"))
        ));

        let peer = PeerFixture::start(Script::Complete).await;
        for reason in [ExactTargetRejectionReason::Unspecified as i32, 99] {
            let attempt = rejected_attempt(
                &peer.target,
                ExactTargetRejected {
                    reason,
                    ..Default::default()
                },
            );
            assert!(
                !attempt.allows_fallback(),
                "reason {reason} must fail closed"
            );
        }
        assert!(
            rejected_attempt(
                &peer.target,
                ExactTargetRejected {
                    reason: ExactTargetRejectionReason::BindingUnavailable as i32,
                    ..Default::default()
                },
            )
            .allows_fallback()
        );
    }

    #[tokio::test]
    async fn the_manager_node_target_is_invoked_in_process_without_a_private_transport() {
        let node = Fixture::serving(Script::Complete);
        let client = PeerInferenceClient::new(None, node.admission());
        let target = in_process_target(&node.binding_token);

        let attempt = timeout(
            Duration::from_secs(10),
            invoke(
                &client,
                target,
                InferenceRequest::Chat(canonical_request()),
                uncancelled(),
            ),
        )
        .await
        .expect("in-process attempt finishes promptly");

        assert!(matches!(
            attempt,
            AdapterAttempt::Response(AdapterResponse::Complete(_))
        ));
        assert_eq!(node.invoker.requests(), [canonical_request()]);
    }

    #[tokio::test]
    async fn an_in_process_refusal_is_safe_to_fall_back_without_engine_work() {
        let node = Fixture::serving(Script::Complete);
        let client = PeerInferenceClient::new(None, node.admission());

        let attempt = invoke(
            &client,
            in_process_target(&"0".repeat(64)),
            InferenceRequest::Chat(canonical_request()),
            uncancelled(),
        )
        .await;

        assert_refused_before_dispatch(attempt, "node rejected the exact target before dispatch");
        assert!(node.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_remote_target_without_a_private_transport_is_refused_before_dispatch() {
        let node = Fixture::serving(Script::Complete);
        let client = PeerInferenceClient::new(None, node.admission());
        let mut target = in_process_target(&node.binding_token);
        target = NodeServingTarget::new(
            target.deployment_id.clone(),
            target.lifecycle_generation,
            target.site_node_id.clone(),
            target.model_id.clone(),
            None,
            std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
            NodeDispatch {
                transport: NodeTransport::Remote {
                    address: "127.0.0.2:47123".parse().expect("worker address"),
                },
                ..target.dispatch().clone()
            },
        );

        let attempt = invoke(
            &client,
            target,
            InferenceRequest::Chat(canonical_request()),
            uncancelled(),
        )
        .await;

        assert_refused_before_dispatch(
            attempt,
            "the manager has no private listener to reach other nodes",
        );
        assert!(node.invoker.requests().is_empty());
    }

    /// The fixture's exact target as the projector publishes it for the node sharing the
    /// manager process.
    fn in_process_target(binding_token: &str) -> NodeServingTarget {
        NodeServingTarget::new(
            ModelDeploymentId(DEPLOYMENT.to_owned()),
            GENERATION,
            WORKER_NODE_ID.to_owned(),
            dotcore_catalog::CatalogModelId("model-a".to_owned()),
            None,
            std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
            NodeDispatch {
                transport: NodeTransport::InProcess,
                manager_session: Fixture::manager_session(),
                node_incarnation: Fixture::node_incarnation(),
                binding_token: binding_token.to_owned(),
            },
        )
    }

    #[tokio::test]
    async fn a_worker_rejection_is_safe_to_fall_back_without_engine_work() {
        let mut peer = PeerFixture::start(Script::Complete).await;
        peer.target = peer.target_with(|dispatch| dispatch.binding_token = "0".repeat(64));

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert_refused_before_dispatch(attempt, "worker rejected the exact target before dispatch");
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn an_unreachable_worker_is_safe_to_fall_back_and_its_address_stays_private() {
        let mut peer = PeerFixture::start(Script::Complete).await;
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let unreachable = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));
        peer.target = peer.target_with(|dispatch| {
            dispatch.transport = NodeTransport::Remote {
                address: unreachable.into(),
            }
        });

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        let error = assert_refused_before_dispatch(
            attempt,
            "worker refused or was unreachable before dispatch",
        );
        assert!(!error.contains(&unreachable.to_string()));
        assert!(!error.contains(&unreachable.port().to_string()));
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_worker_certificate_other_than_the_pinned_one_is_refused_before_dispatch() {
        let peer = PeerFixture::start(Script::Complete).await;
        let (impostor, _) = peer.authority.issue(SITE_ID, WORKER_NODE_ID, true);
        let pinned_elsewhere = peer.client_pinning(&impostor);

        let attempt = invoke(
            &pinned_elsewhere,
            peer.target.clone(),
            InferenceRequest::Chat(canonical_request()),
            uncancelled(),
        )
        .await;

        assert_refused_before_dispatch(
            attempt,
            "worker refused or was unreachable before dispatch",
        );
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_manager_certificate_the_worker_does_not_pin_is_refused_before_dispatch() {
        let peer = PeerFixture::start(Script::Complete).await;
        let unpinned_manager = PeerInferenceClient::new(
            Some(WorkerChannels::new(
                &peer.authority.private_configuration(
                    SITE_ID,
                    MANAGER_NODE_ID,
                    PrivateNodeRole::Manager,
                    peer.authority.issue(SITE_ID, MANAGER_NODE_ID, false),
                    [(
                        WORKER_NODE_ID,
                        PrivateNodeRole::Worker,
                        peer.worker_certificate.as_slice(),
                    )],
                ),
                &dotcore_domain::timing::Timing::production().private,
            )),
            Fixture::installed(Script::Complete).admission(),
        );

        let attempt = invoke(
            &unpinned_manager,
            peer.target.clone(),
            InferenceRequest::Chat(canonical_request()),
            uncancelled(),
        )
        .await;

        assert_refused_before_dispatch(
            attempt,
            "worker refused or was unreachable before dispatch",
        );
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_target_on_a_worker_without_a_pin_is_refused_before_dispatch() {
        let mut peer = PeerFixture::start(Script::Complete).await;
        peer.target = NodeServingTarget::new(
            peer.target.deployment_id.clone(),
            GENERATION,
            "worker-two".to_owned(),
            peer.target.model_id.clone(),
            None,
            std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
            peer.target.dispatch().clone(),
        );

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert_refused_before_dispatch(attempt, "worker channel is unavailable");
        assert!(peer.worker.invoker.requests().is_empty());
    }

    #[tokio::test]
    async fn a_decision_round_trips_to_the_worker_and_returns_its_result() {
        let peer = PeerFixture::start(Script::Complete).await;

        let attempt = peer
            .invoke_request(
                InferenceRequest::Decision(decision_request()),
                uncancelled(),
            )
            .await;

        let AdapterAttempt::Response(AdapterResponse::Decision(result)) = attempt else {
            panic!("the worker decision result must be returned");
        };
        assert_eq!(result, decision_result());
        assert_eq!(
            peer.worker.invoker.invoked_requests(),
            [InferenceRequest::Decision(decision_request())]
        );
    }

    #[tokio::test]
    async fn a_worker_decision_engine_rejection_passes_through_and_failures_fall_back() {
        let passthrough = PeerFixture::start(Script::Passthrough).await;
        let unsupported = PeerFixture::start(Script::UnsupportedRequestKind).await;
        let decision = || InferenceRequest::Decision(decision_request());

        let AdapterAttempt::Response(AdapterResponse::Upstream(response)) =
            passthrough.invoke_request(decision(), uncancelled()).await
        else {
            panic!("the worker passthrough must be returned");
        };
        assert_eq!(
            (response.status, response.body),
            (400, b"{\"error\":\"bad\"}".to_vec())
        );
        let attempt = unsupported.invoke_request(decision(), uncancelled()).await;
        assert!(attempt.allows_fallback());
        let Err(error) = attempt.into_result() else {
            panic!("an unserved request kind must be refused");
        };
        // The worker's refusal reads like a local one.
        assert_eq!(
            crate::router::adapters::failure_stage(&error),
            "unsupported_request_kind"
        );
    }

    #[tokio::test]
    async fn a_worker_context_length_rejection_reaches_the_client_as_a_local_one_would() {
        let peer =
            PeerFixture::start(Script::InvalidRequest(InvalidRequest::context_length())).await;

        let attempt = peer
            .invoke_request(
                InferenceRequest::Decision(decision_request()),
                uncancelled(),
            )
            .await;

        assert!(!attempt.allows_fallback());
        let Err(AdapterError::InvalidRequest(remote)) = attempt.into_result() else {
            panic!("the worker's context length rejection must be an invalid request");
        };
        assert_eq!(remote, InvalidRequest::context_length());
        let local = openai::request_error(InvalidRequest::context_length());
        let remote = openai::request_error(remote);
        assert_eq!(remote.status, 400);
        assert_eq!((remote.status, remote.body), (local.status, local.body));
    }

    #[tokio::test]
    async fn a_worker_engine_refusal_before_output_is_safe_to_fall_back() {
        let peer = PeerFixture::start(Script::RefuseConnection).await;

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert_refused_before_dispatch(attempt, "worker engine attempt failed");
        assert_eq!(peer.worker.invoker.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_failure_after_stream_output_is_started_and_attempted_once() {
        let started = Event::Started(ChatStreamStarted {
            completion_id: "completion-a".to_owned(),
            model_id: "route-a".to_owned(),
            created_at_unix_seconds: 7,
        });
        let peer = PeerFixture::start(Script::StreamThenFail(vec![started.clone()])).await;

        let attempt = peer.invoke(stream_request(), uncancelled()).await;

        assert!(!attempt.allows_fallback());
        let AdapterAttempt::Response(AdapterResponse::Stream(stream)) = attempt else {
            panic!("output commits the stream");
        };
        let items = collect(stream).await;
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].as_ref().expect("forwarded event"),
            &ChatStreamEvent {
                event: Some(started)
            }
        );
        assert!(matches!(
            &items[1],
            Err(AdapterError::Upstream(source))
                if source.to_string() == "worker engine attempt failed"
        ));
        assert_eq!(peer.worker.invoker.requests().len(), 1);
    }

    #[tokio::test]
    async fn a_client_disconnect_during_a_completion_drops_the_call_and_cancels_the_worker() {
        let peer = PeerFixture::start(Script::AwaitCancellation).await;
        let (cancellation, client, _route) = cancellation_with_senders();
        let attempt = tokio::spawn({
            let client = peer.client.clone();
            let target = peer.target.clone();
            async move {
                invoke(
                    &client,
                    target,
                    InferenceRequest::Chat(canonical_request()),
                    cancellation,
                )
                .await
            }
        });
        timeout(
            Duration::from_secs(5),
            peer.worker.invoker.invoked.notified(),
        )
        .await
        .expect("worker engine attempt starts");

        client.send_replace(Some(RequestCancellationReason::ClientDisconnected));

        let attempt = timeout(Duration::from_secs(5), attempt)
            .await
            .expect("adapter returns promptly")
            .expect("adapter task");
        assert!(!attempt.allows_fallback());
        assert!(matches!(
            attempt,
            AdapterAttempt::Failed {
                error: AdapterError::Cancelled(RequestCancellationReason::ClientDisconnected),
                ..
            }
        ));
        assert_eq!(
            peer.worker.invoker.cancellation().await,
            RequestCancellationReason::ClientDisconnected
        );
    }

    #[tokio::test]
    async fn a_revoked_manager_lease_during_a_stream_drops_the_call_and_cancels_the_worker() {
        let started = Event::Started(ChatStreamStarted {
            completion_id: "completion-a".to_owned(),
            model_id: "route-a".to_owned(),
            created_at_unix_seconds: 7,
        });
        let peer =
            PeerFixture::start(Script::StreamThenAwaitCancellation(vec![started.clone()])).await;
        let (cancellation, _client, route) = cancellation_with_senders();
        let attempt = peer.invoke(stream_request(), cancellation).await;
        let AdapterAttempt::Response(AdapterResponse::Stream(mut stream)) = attempt else {
            panic!("the worker stream must be returned");
        };
        assert_eq!(
            next_item(&mut stream)
                .await
                .map(|item| item.expect("forwarded event")),
            Some(ChatStreamEvent {
                event: Some(started)
            })
        );

        route.send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));

        assert!(matches!(
            next_item(&mut stream).await,
            Some(Err(AdapterError::Cancelled(
                RequestCancellationReason::RouteRevoked
            )))
        ));
        assert!(next_item(&mut stream).await.is_none());
        assert_eq!(
            peer.worker.invoker.cancellation().await,
            RequestCancellationReason::ClientDisconnected,
            "the worker observes the dropped call"
        );
    }

    #[tokio::test]
    async fn one_connection_per_worker_is_reused_until_the_worker_address_changes() {
        let mut peer = PeerFixture::start(Script::Complete).await;
        let first = CountingProxy::start(peer.worker_address).await;
        peer.target = peer.target_with(|dispatch| {
            dispatch.transport = NodeTransport::Remote {
                address: first.address.into(),
            }
        });

        for _ in 0..3 {
            let attempt = peer.invoke(canonical_request(), uncancelled()).await;
            assert!(matches!(
                attempt,
                AdapterAttempt::Response(AdapterResponse::Complete(_))
            ));
        }
        let second = CountingProxy::start(peer.worker_address).await;
        peer.target = peer.target_with(|dispatch| {
            dispatch.transport = NodeTransport::Remote {
                address: second.address.into(),
            }
        });
        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert!(matches!(
            attempt,
            AdapterAttempt::Response(AdapterResponse::Complete(_))
        ));
        assert_eq!(
            (first.connections(), second.connections()),
            (1, 1),
            "three calls share one connection; a new address opens one more"
        );
        assert_eq!(
            peer.client
                .channels
                .as_ref()
                .expect("private transport")
                .channel_addresses(WorkerChannelPurpose::Inference),
            [(WORKER_NODE_ID.to_owned(), second.address.into())]
        );
        assert_eq!(peer.worker.invoker.requests().len(), 4);
    }

    #[tokio::test]
    async fn a_worker_declared_by_name_is_reached_at_the_ipv4_address_that_accepts() {
        let mut peer = PeerFixture::start(Script::Complete).await;
        let port = peer.worker_address.port();
        assert_eq!(
            peer.worker_address.ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        // `localhost` commonly resolves to `::1` first, where nothing listens at this port, so
        // the connector must move on to `127.0.0.1`.
        assert!(tokio::net::TcpStream::connect(("::1", port)).await.is_err());
        peer.target = peer.target_with(|dispatch| {
            dispatch.transport = NodeTransport::Remote {
                address: format!("localhost:{port}").parse().expect("named worker"),
            }
        });

        let attempt = peer.invoke(canonical_request(), uncancelled()).await;

        assert!(matches!(
            attempt,
            AdapterAttempt::Response(AdapterResponse::Complete(_))
        ));
    }

    /// A worker serving the fixture's exact target over real mutual TLS, and a manager client
    /// pinned to it.
    struct PeerFixture {
        worker: Fixture,
        authority: TestAuthority,
        worker_address: SocketAddr,
        worker_certificate: Vec<u8>,
        client: PeerInferenceClient,
        manager_certificate: Vec<u8>,
        manager_key: Vec<u8>,
        target: NodeServingTarget,
    }

    impl PeerFixture {
        async fn start(script: Script) -> Self {
            let worker = Fixture::serving(script);
            let authority = TestAuthority::new();
            let (worker_certificate, worker_key) = authority.issue(SITE_ID, WORKER_NODE_ID, true);
            let (manager_certificate, manager_key) =
                authority.issue(SITE_ID, MANAGER_NODE_ID, false);
            let server = crate::server::private::prepare(
                authority.private_configuration(
                    SITE_ID,
                    WORKER_NODE_ID,
                    PrivateNodeRole::Worker,
                    (worker_certificate.clone(), worker_key),
                    [(
                        MANAGER_NODE_ID,
                        PrivateNodeRole::Manager,
                        manager_certificate.as_slice(),
                    )],
                ),
                &dotcore_domain::timing::Timing::production().private,
                worker
                    .private_services()
                    .assemble(PrivateNodeRole::Worker, WORKER_NODE_ID),
                std::future::pending(),
            )
            .await
            .expect("worker private listener");
            let worker_address = server.address;
            tokio::spawn(server.serve());
            Self {
                target: NodeServingTarget::new(
                    ModelDeploymentId(DEPLOYMENT.to_owned()),
                    GENERATION,
                    WORKER_NODE_ID.to_owned(),
                    dotcore_catalog::CatalogModelId("model-a".to_owned()),
                    None,
                    std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
                    NodeDispatch {
                        transport: NodeTransport::Remote {
                            address: worker_address.into(),
                        },
                        manager_session: Fixture::manager_session(),
                        node_incarnation: Fixture::node_incarnation(),
                        binding_token: worker.binding_token.clone(),
                    },
                ),
                client: PeerInferenceClient::new(
                    Some(WorkerChannels::new(
                        &authority.private_configuration(
                            SITE_ID,
                            MANAGER_NODE_ID,
                            PrivateNodeRole::Manager,
                            (manager_certificate.clone(), manager_key.clone()),
                            [(
                                WORKER_NODE_ID,
                                PrivateNodeRole::Worker,
                                worker_certificate.as_slice(),
                            )],
                        ),
                        &dotcore_domain::timing::Timing::production().private,
                    )),
                    Fixture::installed(Script::Complete).admission(),
                ),
                worker,
                authority,
                worker_address,
                worker_certificate,
                manager_certificate,
                manager_key,
            }
        }

        /// A manager client that pins `worker_certificate` as the worker's exact certificate.
        fn client_pinning(&self, worker_certificate: &[u8]) -> PeerInferenceClient {
            PeerInferenceClient::new(
                Some(WorkerChannels::new(
                    &self.authority.private_configuration(
                        SITE_ID,
                        MANAGER_NODE_ID,
                        PrivateNodeRole::Manager,
                        (self.manager_certificate.clone(), self.manager_key.clone()),
                        [(WORKER_NODE_ID, PrivateNodeRole::Worker, worker_certificate)],
                    ),
                    &dotcore_domain::timing::Timing::production().private,
                )),
                Fixture::installed(Script::Complete).admission(),
            )
        }

        fn target_with(&self, change: impl FnOnce(&mut NodeDispatch)) -> NodeServingTarget {
            let mut dispatch = self.target.dispatch().clone();
            change(&mut dispatch);
            NodeServingTarget::new(
                self.target.deployment_id.clone(),
                self.target.lifecycle_generation,
                self.target.site_node_id.clone(),
                self.target.model_id.clone(),
                None,
                std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
                dispatch,
            )
        }

        async fn invoke(
            &self,
            request: ChatRequest,
            cancellation: RequestCancellation,
        ) -> AdapterAttempt {
            self.invoke_request(InferenceRequest::Chat(request), cancellation)
                .await
        }

        async fn invoke_request(
            &self,
            request: InferenceRequest,
            cancellation: RequestCancellation,
        ) -> AdapterAttempt {
            timeout(
                Duration::from_secs(10),
                invoke(&self.client, self.target.clone(), request, cancellation),
            )
            .await
            .expect("peer attempt finishes promptly")
        }
    }

    /// Loopback TCP relay to the worker that counts the connections the client opens.
    struct CountingProxy {
        address: SocketAddr,
        connections: Arc<AtomicUsize>,
    }

    impl CountingProxy {
        async fn start(upstream: SocketAddr) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("proxy listener");
            let address = listener.local_addr().expect("proxy address");
            let connections = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&connections);
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    counted.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let mut outbound = TcpStream::connect(upstream).await.expect("worker");
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                    });
                }
            });
            Self {
                address,
                connections,
            }
        }

        fn connections(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
    }

    /// Asserts a safe-to-fall-back pre-dispatch refusal and returns its safe message.
    fn assert_refused_before_dispatch(attempt: AdapterAttempt, message: &str) -> String {
        assert!(attempt.allows_fallback(), "refusal must allow fallback");
        let Err(error) = attempt.into_result() else {
            panic!("the attempt must be refused");
        };
        let AdapterError::PreDispatchUnavailable(source) = &error else {
            panic!("expected a pre-dispatch refusal, got {error}");
        };
        assert_eq!(source.to_string(), message);
        error.to_string()
    }

    /// The shared Chat fixture with an image part, as the manager admitted it.
    fn canonical_request() -> ChatRequest {
        let body = serde_json::to_vec(&test_fixtures::chat_request()).expect("fixture JSON");
        let OpenAiRequest::Chat(mut request) =
            openai::decode("POST", "/v1/chat/completions", &body).expect("Chat request")
        else {
            panic!("fixture must decode as Chat");
        };
        request.messages.push(ChatMessage {
            role: ChatMessageRole::User as i32,
            content_parts: vec![ChatContentPart {
                content: Some(chat_content_part::Content::ImageContent(ChatImageContent {
                    media_type: ChatImageMediaType::Png as i32,
                    encoded_data: vec![1, 2, 3],
                })),
            }],
            ..Default::default()
        });
        request
    }

    fn stream_request() -> ChatRequest {
        ChatRequest {
            stream: Some(true),
            ..canonical_request()
        }
    }

    async fn collect(
        mut stream: mpsc::Receiver<Result<ChatStreamEvent, AdapterError>>,
    ) -> Vec<Result<ChatStreamEvent, AdapterError>> {
        let mut items = Vec::new();
        while let Some(item) = next_item(&mut stream).await {
            items.push(item);
        }
        items
    }

    async fn next_item(
        stream: &mut mpsc::Receiver<Result<ChatStreamEvent, AdapterError>>,
    ) -> Option<Result<ChatStreamEvent, AdapterError>> {
        timeout(Duration::from_secs(5), stream.recv())
            .await
            .expect("stream item arrives promptly")
    }

    fn uncancelled() -> RequestCancellation {
        cancellation_with_senders().0
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
}
