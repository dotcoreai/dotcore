//! Serves exact-target inference on a worker's private listener for the authenticated site
//! manager, which forwards requests it already admitted on one of its routes.
//!
//! Each request names one worker realization by manager session, worker incarnation, deployment,
//! lifecycle generation, and binding token. The worker admits it only against its installed view
//! and a live local binding, makes exactly one engine attempt, and streams the outcome. It never
//! looks up routes, resolves model names, or falls back to another target. It also re-validates
//! the canonical request, so an invalid one is refused before any engine sees it and is never
//! retried elsewhere.

use std::pin::Pin;

use dotcore_proto::dotcore::inference::private::v1::{
    ExactServingTarget, ExactTargetAdmitted, ExactTargetFailure, ExactTargetFailureDisposition,
    ExactTargetFailureKind, ExactTargetRejected, ExactTargetRejectionReason,
    InvokeExactTargetRequest, InvokeExactTargetResponse, PassthroughHeader, PassthroughResponse,
    RequestErrorCategory, invoke_exact_target_request,
    invoke_exact_target_response::Frame,
    peer_inference_service_server::{
        PeerInferenceService as PeerInferenceServiceContract, PeerInferenceServiceServer,
    },
};
use dotcore_site::{
    private_transport::{AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext},
    view::fencing::{AcceptedNodeViewAuthority, ServingAuthorizationError},
};
use tokio::sync::{mpsc, watch};
use tonic::{Request, Response, Status};
use tracing::{debug, warn};

use super::{
    adapters::{AdapterAttempt, AdapterAttemptDisposition, AdapterError, AdapterResponse},
    cancellation::{RequestCancellation, RequestCancellationReason},
    request_controls,
};
use crate::{
    inference::{InferenceError, InferenceRequest, InvalidRequest, InvalidRequestCategory},
    model::controller::serving_targets::{
        LocalServingTarget, LocalServingTargetsSource, ServingTargetsLease,
    },
};

/// Largest encoded peer inference message in either direction: a complete public request body
/// plus room for the exact-target identity and protobuf framing.
pub(crate) const PEER_INFERENCE_MESSAGE_LIMIT_BYTES: usize =
    super::http::MAX_OPENAI_REQUEST_BODY_BYTES + 1024 * 1024;

/// Ordered frames of one exact-target attempt as sent to the manager.
type ExactTargetFrames =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<InvokeExactTargetResponse, Status>> + Send>>;

/// Frames buffered toward the manager before the engine stream waits for it to read.
const FRAME_BUFFER: usize = 8;

/// Worker-hosted private receiver for manager-forwarded exact-target inference.
pub(crate) struct PeerInferenceService {
    admission: ExactTargetAdmission,
    authorization: AuthenticatedRoleAuthorization,
}

impl PeerInferenceService {
    /// Creates a receiver authorized for the manager that admits through `admission`.
    pub(crate) fn new(admission: ExactTargetAdmission, context: &PrivateServiceContext) -> Self {
        Self {
            admission,
            authorization: context.authorize_role(PrivateNodeRole::Manager),
        }
    }

    /// Wraps the receiver in its gRPC server with limits sized for complete public requests.
    pub(crate) fn into_server(self) -> PeerInferenceServiceServer<Self> {
        PeerInferenceServiceServer::new(self)
            .max_decoding_message_size(PEER_INFERENCE_MESSAGE_LIMIT_BYTES)
            .max_encoding_message_size(PEER_INFERENCE_MESSAGE_LIMIT_BYTES)
    }

    /// Validates the request and admits its exact target.
    fn admit(&self, request: InvokeExactTargetRequest) -> Result<AdmittedAttempt, Rejection> {
        let (Some(target), Some(request)) = (request.target, request.request) else {
            return Err(Rejection::invalid(InvalidRequest::invalid_field(None)));
        };
        let request = match request {
            invoke_exact_target_request::Request::Chat(request) => InferenceRequest::Chat(request),
            invoke_exact_target_request::Request::Decision(request) => {
                InferenceRequest::Decision(request)
            }
        };
        super::selection::validate_request(&request).map_err(Rejection::invalid)?;
        let (target, lease) = self
            .admission
            .admit(&target)
            .map_err(Rejection::unavailable)?;
        Ok(AdmittedAttempt {
            target,
            lease,
            request,
        })
    }
}

/// Node-local admission of one exact serving target, whatever transport carries the request.
///
/// A target is admitted only when the installed view assigns it under the current manager session
/// and node incarnation and a live local binding serves exactly its generation and token; the
/// returned lease keeps that binding until the attempt ends or the binding is revoked.
pub(crate) struct ExactTargetAdmission {
    view: AcceptedNodeViewAuthority,
    targets: LocalServingTargetsSource,
}

impl ExactTargetAdmission {
    /// Creates an admission over the node's installed view and its local serving targets.
    pub(crate) fn new(view: AcceptedNodeViewAuthority, targets: LocalServingTargetsSource) -> Self {
        Self { view, targets }
    }

    /// Admits `target` against the installed view, then leases its live local binding.
    pub(crate) fn admit(
        &self,
        target: &ExactServingTarget,
    ) -> Result<(LocalServingTarget, ServingTargetsLease), ExactTargetRejectionReason> {
        self.view
            .authorize_serving(
                &target.manager_session_id,
                &target.node_incarnation_id,
                &target.model_deployment_id,
                target.lifecycle_generation,
            )
            .map_err(|error| match error {
                ServingAuthorizationError::NodeIncarnationMismatch => {
                    ExactTargetRejectionReason::NodeIncarnationMismatch
                }
                ServingAuthorizationError::ManagerSessionMismatch => {
                    ExactTargetRejectionReason::ManagerSessionMismatch
                }
                ServingAuthorizationError::DeploymentNotAssigned => {
                    ExactTargetRejectionReason::DeploymentNotAssigned
                }
            })?;
        // The source stays empty until the lazily built realizer publishes, so an unbuilt
        // realizer is refused here like any missing binding.
        let unavailable = ExactTargetRejectionReason::BindingUnavailable;
        self.targets
            .admit(|targets| {
                targets
                    .iter()
                    .find(|local| serves(local, target))
                    .map(|local| (local.clone(), local.clone()))
                    .ok_or(unavailable)
            })?
            .ok_or(unavailable)
    }
}

/// Why the worker refused an exact target before dispatch, with invalid-request details.
struct Rejection {
    reason: ExactTargetRejectionReason,
    invalid: Option<InvalidRequest>,
}

impl Rejection {
    fn invalid(error: InvalidRequest) -> Self {
        Self {
            reason: ExactTargetRejectionReason::InvalidRequest,
            invalid: Some(error),
        }
    }

    fn unavailable(reason: ExactTargetRejectionReason) -> Self {
        Self {
            reason,
            invalid: None,
        }
    }

    fn frame(&self) -> ExactTargetRejected {
        ExactTargetRejected {
            reason: self.reason as i32,
            safe_param: self
                .invalid
                .and_then(|invalid| invalid.param)
                .map(str::to_owned),
            request_error_category: self
                .invalid
                .map(|invalid| peer_request_error_category(invalid.category) as i32),
        }
    }
}

/// Encodes a safe invalid-request category for the private peer contract.
pub(crate) fn peer_request_error_category(
    category: InvalidRequestCategory,
) -> RequestErrorCategory {
    match category {
        InvalidRequestCategory::InvalidField => RequestErrorCategory::InvalidField,
        InvalidRequestCategory::UnsupportedFeature => RequestErrorCategory::UnsupportedFeature,
        InvalidRequestCategory::RequestLimit => RequestErrorCategory::RequestLimit,
        InvalidRequestCategory::ContextLength => RequestErrorCategory::ContextLength,
    }
}

/// Decodes peer invalid-request details, keeping only a recognized field and known category.
pub(crate) fn peer_invalid_request(category: Option<i32>, param: Option<&str>) -> InvalidRequest {
    let category = match category.map(RequestErrorCategory::try_from) {
        Some(Ok(RequestErrorCategory::UnsupportedFeature)) => {
            InvalidRequestCategory::UnsupportedFeature
        }
        Some(Ok(RequestErrorCategory::RequestLimit)) => InvalidRequestCategory::RequestLimit,
        Some(Ok(RequestErrorCategory::ContextLength)) => InvalidRequestCategory::ContextLength,
        Some(Ok(RequestErrorCategory::InvalidField | RequestErrorCategory::Unspecified))
        | Some(Err(_))
        | None => InvalidRequestCategory::InvalidField,
    };
    InvalidRequest {
        category,
        param: param.and_then(request_controls::recognized_param),
    }
}

#[tonic::async_trait]
impl PeerInferenceServiceContract for PeerInferenceService {
    type InvokeExactTargetStream = ExactTargetFrames;

    async fn invoke_exact_target(
        &self,
        request: Request<InvokeExactTargetRequest>,
    ) -> Result<Response<Self::InvokeExactTargetStream>, Status> {
        if self.authorization.receiver_role() != PrivateNodeRole::Worker {
            return Err(Status::permission_denied(
                "exact-target inference is served only by workers",
            ));
        }
        self.authorization.authorize(&request)?;
        let request = request.into_inner();
        let deployment_id = request
            .target
            .as_ref()
            .map(|target| target.model_deployment_id.clone())
            .unwrap_or_default();
        let (frames, receiver) = mpsc::channel(FRAME_BUFFER);
        match self.admit(request) {
            Ok(attempt) => {
                debug!(
                    event_kind = "peer_inference_admission",
                    outcome = "admitted",
                    deployment_id,
                    "Exact-target inference admitted"
                );
                tokio::spawn(attempt.run(frames));
            }
            Err(rejection) => {
                warn!(
                    event_kind = "peer_inference_admission",
                    outcome = "rejected",
                    deployment_id,
                    reason = rejection.reason.as_str_name(),
                    param = rejection
                        .invalid
                        .and_then(|invalid| invalid.param)
                        .unwrap_or("unspecified"),
                    "Exact-target inference rejected before dispatch"
                );
                frames
                    .try_send(Ok(frame(Frame::Rejected(rejection.frame()))))
                    .expect("an empty frame buffer accepts the rejection");
            }
        }
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(receiver),
        )))
    }
}

/// One admitted exact target holding its node-local binding lease.
struct AdmittedAttempt {
    target: LocalServingTarget,
    lease: ServingTargetsLease,
    request: InferenceRequest,
}

impl AdmittedAttempt {
    /// Sends Admitted, makes the single engine attempt, and streams its outcome.
    ///
    /// A closed frame stream means the manager dropped the RPC, which cancels the attempt as a
    /// client disconnect; revoking the lease cancels it as a route revocation.
    async fn run(self, frames: mpsc::Sender<Result<InvokeExactTargetResponse, Status>>) {
        let deployment_id = self.target.deployment_id.0.clone();
        if send(&frames, Frame::Admitted(ExactTargetAdmitted {}))
            .await
            .is_err()
        {
            return;
        }
        let (client, client_cancellation) = watch::channel(None);
        let (_, route) = self.lease.into_parts();
        let mut cancellation = RequestCancellation::new(client_cancellation, route);
        let invoke =
            super::adapters::invoke_local_binding(self.target, self.request, cancellation.clone());
        tokio::pin!(invoke);
        let attempt = tokio::select! {
            attempt = &mut invoke => attempt,
            () = frames.closed() => {
                client.send_replace(Some(RequestCancellationReason::ClientDisconnected));
                invoke.await
            }
        };
        let outcome = match attempt {
            AdapterAttempt::Response(AdapterResponse::Stream(mut events)) => loop {
                let event = tokio::select! {
                    biased;
                    reason = cancellation.cancelled() => {
                        break cancelled(&frames, reason).await;
                    }
                    () = frames.closed() => {
                        client.send_replace(Some(RequestCancellationReason::ClientDisconnected));
                        break "manager_disconnected";
                    }
                    event = events.recv() => event,
                };
                match event {
                    Some(Ok(event)) => {
                        if send(&frames, Frame::StreamEvent(event)).await.is_err() {
                            client
                                .send_replace(Some(RequestCancellationReason::ClientDisconnected));
                            break "manager_disconnected";
                        }
                    }
                    Some(Err(error)) => {
                        break failed(
                            &frames,
                            AdapterAttemptDisposition::StartedOrAmbiguous,
                            &error,
                        )
                        .await;
                    }
                    None => break "stream_completed",
                }
            },
            AdapterAttempt::Response(response) => match cancellation.reason() {
                Some(reason) => cancelled(&frames, reason).await,
                None => {
                    let _ = send(&frames, response_frame(response)).await;
                    "completed"
                }
            },
            AdapterAttempt::Failed { disposition, error } => {
                failed(&frames, disposition, &error).await
            }
        };
        debug!(
            event_kind = "peer_inference_attempt",
            outcome, deployment_id, "Exact-target inference attempt finished"
        );
    }
}

/// Whether a live local target is exactly the requested realization.
fn serves(local: &LocalServingTarget, target: &ExactServingTarget) -> bool {
    local.deployment_id.0 == target.model_deployment_id
        && local.lifecycle_generation == target.lifecycle_generation
        && local.binding_token() == target.binding_token
}

fn response_frame(response: AdapterResponse) -> Frame {
    match response {
        AdapterResponse::Complete(result) => Frame::Completion(result),
        AdapterResponse::Decision(result) => Frame::DecisionResult(result),
        AdapterResponse::Upstream(response) => Frame::Passthrough(PassthroughResponse {
            status: u32::from(response.status),
            headers: response
                .headers
                .into_iter()
                .map(|(name, value)| PassthroughHeader { name, value })
                .collect(),
            body: response.body,
        }),
        AdapterResponse::Stream(_) => unreachable!("streams are forwarded event by event"),
    }
}

async fn cancelled(
    frames: &mpsc::Sender<Result<InvokeExactTargetResponse, Status>>,
    reason: RequestCancellationReason,
) -> &'static str {
    failed(
        frames,
        AdapterAttemptDisposition::StartedOrAmbiguous,
        &AdapterError::Cancelled(reason),
    )
    .await
}

async fn failed(
    frames: &mpsc::Sender<Result<InvokeExactTargetResponse, Status>>,
    disposition: AdapterAttemptDisposition,
    error: &AdapterError,
) -> &'static str {
    let kind = match error {
        AdapterError::Cancelled(RequestCancellationReason::ClientDisconnected) => {
            ExactTargetFailureKind::ClientDisconnected
        }
        AdapterError::Cancelled(RequestCancellationReason::RequestTimedOut) => {
            ExactTargetFailureKind::RequestTimedOut
        }
        AdapterError::Cancelled(RequestCancellationReason::RouteRevoked) => {
            ExactTargetFailureKind::RouteRevoked
        }
        AdapterError::PreDispatchUnavailable(source)
            if matches!(
                source.downcast_ref::<InferenceError>(),
                Some(InferenceError::UnsupportedRequestKind)
            ) =>
        {
            ExactTargetFailureKind::UnsupportedRequestKind
        }
        AdapterError::Upstream(_) | AdapterError::PreDispatchUnavailable(_) => {
            ExactTargetFailureKind::Provider
        }
        AdapterError::UnsupportedCapability => ExactTargetFailureKind::UnsupportedCapability,
        AdapterError::InvalidRequest(_) => ExactTargetFailureKind::InvalidRequest,
    };
    let invalid = match error {
        AdapterError::InvalidRequest(invalid) => Some(*invalid),
        _ => None,
    };
    let disposition = match disposition {
        _ if invalid.is_some() => ExactTargetFailureDisposition::NonRetryable,
        AdapterAttemptDisposition::SafeToFallback => ExactTargetFailureDisposition::SafeToFallback,
        AdapterAttemptDisposition::StartedOrAmbiguous => {
            ExactTargetFailureDisposition::StartedOrAmbiguous
        }
        AdapterAttemptDisposition::NonRetryable => ExactTargetFailureDisposition::NonRetryable,
    };
    if !matches!(error, AdapterError::Cancelled(_)) {
        warn!(
            event_kind = "peer_inference_attempt",
            outcome = "failed",
            disposition = disposition.as_str_name(),
            kind = kind.as_str_name(),
            failure_stage = super::adapters::failure_stage(error),
            "Exact-target inference attempt failed"
        );
    }
    let _ = send(
        frames,
        Frame::Failure(ExactTargetFailure {
            disposition: disposition as i32,
            kind: kind as i32,
            safe_param: invalid.and_then(|invalid| invalid.param).map(str::to_owned),
            request_error_category: invalid
                .map(|invalid| peer_request_error_category(invalid.category) as i32),
        }),
    )
    .await;
    if matches!(error, AdapterError::Cancelled(_)) {
        "cancelled"
    } else {
        "failed"
    }
}

async fn send(
    frames: &mpsc::Sender<Result<InvokeExactTargetResponse, Status>>,
    payload: Frame,
) -> Result<(), mpsc::error::SendError<()>> {
    frames
        .send(Ok(frame(payload)))
        .await
        .map_err(|_| mpsc::error::SendError(()))
}

fn frame(payload: Frame) -> InvokeExactTargetResponse {
    InvokeExactTargetResponse {
        frame: Some(payload),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatReasoningDelta, ChatStreamEvent, ChatStreamStarted, ChatTextDelta,
        chat_stream_event::Event, chat_text_delta,
    };
    use dotcore_site::private_transport::AuthenticatedMachineIdentity;

    use super::{
        test_support::{
            Fixture, GENERATION, SITE_ID, Script, WORKER_NODE_ID, authenticated, completion,
            decision_request, decision_result, next_frame,
        },
        *,
    };

    #[tokio::test]
    async fn send_reports_a_closed_receiver() {
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);

        assert!(matches!(
            send(&sender, Frame::Admitted(ExactTargetAdmitted {})).await,
            Err(mpsc::error::SendError(()))
        ));
    }

    #[tokio::test]
    async fn admits_the_exact_target_and_returns_its_single_completion() {
        let fixture = Fixture::serving(Script::Complete);

        let frames = fixture.frames(fixture.request()).await;

        assert_eq!(
            frames,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                Frame::Completion(completion()),
            ]
        );
        assert_eq!(fixture.invoker.invoked_models(), ["route-a"]);
    }

    #[tokio::test]
    async fn stale_unknown_or_mismatched_identities_are_rejected_before_dispatch() {
        let fixture = Fixture::serving(Script::Complete);
        let mismatches: [Mismatch; 7] = [
            (
                "missing target",
                |request| request.target = None,
                ExactTargetRejectionReason::InvalidRequest,
            ),
            (
                "missing Chat request",
                |request| request.request = None,
                ExactTargetRejectionReason::InvalidRequest,
            ),
            (
                "previous worker incarnation",
                |request| target(request).node_incarnation_id = "2".repeat(32),
                ExactTargetRejectionReason::NodeIncarnationMismatch,
            ),
            (
                "replaced manager session",
                |request| target(request).manager_session_id = "b".repeat(32),
                ExactTargetRejectionReason::ManagerSessionMismatch,
            ),
            (
                "unknown deployment",
                |request| target(request).model_deployment_id = "deployment-b".to_owned(),
                ExactTargetRejectionReason::DeploymentNotAssigned,
            ),
            (
                "stale lifecycle generation",
                |request| target(request).lifecycle_generation = GENERATION - 1,
                ExactTargetRejectionReason::DeploymentNotAssigned,
            ),
            (
                "replaced binding token",
                |request| target(request).binding_token = "0".repeat(64),
                ExactTargetRejectionReason::BindingUnavailable,
            ),
        ];

        for (case, mismatch, reason) in mismatches {
            let mut request = fixture.request();
            mismatch(&mut request);
            let expected = match reason {
                // A structurally incomplete request is an invalid request, not an availability miss.
                ExactTargetRejectionReason::InvalidRequest => {
                    Frame::Rejected(ExactTargetRejected {
                        reason: reason as i32,
                        safe_param: None,
                        request_error_category: Some(RequestErrorCategory::InvalidField as i32),
                    })
                }
                reason => rejected(reason),
            };
            assert_eq!(
                fixture.frames(request).await,
                [expected],
                "{case} must be rejected"
            );
        }
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    #[tokio::test]
    async fn requests_are_rejected_until_the_realizer_publishes_a_binding() {
        let fixture = Fixture::installed(Script::Complete);

        assert_eq!(
            fixture.frames(fixture.request()).await,
            [rejected(ExactTargetRejectionReason::BindingUnavailable)]
        );
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    #[tokio::test]
    async fn only_the_authenticated_manager_may_invoke_and_only_on_a_worker() {
        let fixture = Fixture::serving(Script::Complete);

        let worker = fixture
            .call(fixture.request(), Some(PrivateNodeRole::Worker))
            .await
            .err()
            .expect("a worker caller is refused");
        let anonymous = fixture
            .call(fixture.request(), None)
            .await
            .err()
            .expect("an unauthenticated caller is refused");
        let on_manager = PeerInferenceService::new(
            ExactTargetAdmission::new(fixture.view.clone(), fixture.publisher.source()),
            &PrivateServiceContext::new(PrivateNodeRole::Manager, "manager-one"),
        )
        .invoke_exact_target(authenticated(
            fixture.request(),
            Some(PrivateNodeRole::Manager),
        ))
        .await
        .err()
        .expect("a manager does not host exact-target inference");

        assert_eq!(
            (worker.code(), worker.message()),
            (
                tonic::Code::PermissionDenied,
                "private operation is not authorized"
            )
        );
        assert_eq!(anonymous.code(), tonic::Code::Unauthenticated);
        assert_eq!(on_manager.code(), tonic::Code::PermissionDenied);
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    #[tokio::test]
    async fn a_caller_claiming_the_receiving_site_node_is_refused() {
        let fixture = Fixture::serving(Script::Complete);
        let mut request = authenticated(fixture.request(), None);
        request
            .extensions_mut()
            .insert(AuthenticatedMachineIdentity {
                site_id: SITE_ID.to_owned(),
                node_id: WORKER_NODE_ID.to_owned(),
                role: PrivateNodeRole::Manager,
            });

        let error = fixture
            .service
            .invoke_exact_target(request)
            .await
            .err()
            .expect("a caller claiming the receiving node is refused");

        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::PermissionDenied,
                "private operation is not authorized"
            )
        );
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    #[tokio::test]
    async fn engine_failure_before_output_keeps_its_safe_to_fallback_disposition() {
        let fixture = Fixture::serving(Script::RefuseConnection);

        assert_eq!(
            fixture.frames(fixture.request()).await,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                failure(
                    ExactTargetFailureDisposition::SafeToFallback,
                    ExactTargetFailureKind::Provider
                ),
            ]
        );
        assert_eq!(fixture.invoker.invoked_models().len(), 1);
    }

    #[tokio::test]
    async fn stream_events_are_forwarded_in_order_until_a_mid_stream_failure() {
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
        ];
        let fixture = Fixture::serving(Script::StreamThenFail(events.clone()));

        let mut expected = vec![Frame::Admitted(ExactTargetAdmitted {})];
        expected.extend(
            events
                .into_iter()
                .map(|event| Frame::StreamEvent(ChatStreamEvent { event: Some(event) })),
        );
        expected.push(failure(
            ExactTargetFailureDisposition::StartedOrAmbiguous,
            ExactTargetFailureKind::Provider,
        ));
        assert_eq!(fixture.frames(fixture.request()).await, expected);
    }

    #[tokio::test]
    async fn passthrough_response_is_forwarded_unchanged() {
        let fixture = Fixture::serving(Script::Passthrough);

        assert_eq!(
            fixture.frames(fixture.request()).await,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                Frame::Passthrough(PassthroughResponse {
                    status: 400,
                    headers: vec![PassthroughHeader {
                        name: "content-type".to_owned(),
                        value: "application/json".to_owned(),
                    }],
                    body: b"{\"error\":\"bad\"}".to_vec(),
                }),
            ]
        );
    }

    #[tokio::test]
    async fn dropping_the_rpc_cancels_a_long_completion_as_client_disconnected() {
        let fixture = Fixture::serving(Script::AwaitCancellation);
        let mut stream = fixture
            .call(fixture.request(), Some(PrivateNodeRole::Manager))
            .await
            .expect("admitted call")
            .into_inner();
        assert_eq!(
            next_frame(&mut stream).await,
            Some(Frame::Admitted(ExactTargetAdmitted {}))
        );
        fixture.invoker.invoked.notified().await;

        drop(stream);

        assert_eq!(
            fixture.invoker.cancellation().await,
            RequestCancellationReason::ClientDisconnected
        );
    }

    #[tokio::test]
    async fn revoking_the_worker_binding_lease_cancels_the_attempt_as_route_revoked() {
        let fixture = Fixture::serving(Script::AwaitCancellation);
        let mut stream = fixture
            .call(fixture.request(), Some(PrivateNodeRole::Manager))
            .await
            .expect("admitted call")
            .into_inner();
        assert_eq!(
            next_frame(&mut stream).await,
            Some(Frame::Admitted(ExactTargetAdmitted {}))
        );
        fixture.invoker.invoked.notified().await;

        fixture.publisher.publish(Vec::new());

        assert_eq!(
            next_frame(&mut stream).await,
            Some(failure(
                ExactTargetFailureDisposition::StartedOrAmbiguous,
                ExactTargetFailureKind::RouteRevoked
            ))
        );
        assert_eq!(next_frame(&mut stream).await, None);
        assert_eq!(
            fixture.invoker.cancellation().await,
            RequestCancellationReason::RouteRevoked
        );
    }

    type Mismatch = (
        &'static str,
        fn(&mut InvokeExactTargetRequest),
        ExactTargetRejectionReason,
    );

    fn target(request: &mut InvokeExactTargetRequest) -> &mut ExactServingTarget {
        request.target.as_mut().expect("exact target")
    }

    #[tokio::test]
    async fn an_invalid_canonical_request_is_rejected_before_admission_with_safe_details() {
        let fixture = Fixture::serving(Script::Complete);
        for (controls, expected) in [
            (
                r#"{"api_key":"sk-sentinel"}"#,
                ExactTargetRejected {
                    reason: ExactTargetRejectionReason::InvalidRequest as i32,
                    safe_param: None,
                    request_error_category: Some(RequestErrorCategory::InvalidField as i32),
                },
            ),
            (
                r#"{"response_format":{"type":"json_object"}}"#,
                ExactTargetRejected {
                    reason: ExactTargetRejectionReason::InvalidRequest as i32,
                    safe_param: Some("response_format".to_owned()),
                    request_error_category: Some(RequestErrorCategory::UnsupportedFeature as i32),
                },
            ),
        ] {
            let mut request = fixture.request();
            let Some(invoke_exact_target_request::Request::Chat(chat)) = request.request.as_mut()
            else {
                panic!("chat request");
            };
            chat.request_extensions_json = Some(controls.to_owned());

            assert_eq!(
                fixture.frames(request).await,
                [Frame::Rejected(expected)],
                "{controls}"
            );
        }
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    #[tokio::test]
    async fn a_binding_request_error_after_admission_is_a_non_retryable_failure() {
        let fixture = Fixture::serving(Script::InvalidRequest(InvalidRequest::invalid_field(
            Some("top_k"),
        )));

        let frames = fixture.frames(fixture.request()).await;

        assert_eq!(
            frames,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                Frame::Failure(ExactTargetFailure {
                    disposition: ExactTargetFailureDisposition::NonRetryable as i32,
                    kind: ExactTargetFailureKind::InvalidRequest as i32,
                    safe_param: Some("top_k".to_owned()),
                    request_error_category: Some(RequestErrorCategory::InvalidField as i32),
                }),
            ]
        );
        assert_eq!(fixture.invoker.invoked_models(), ["route-a"]);
    }

    #[tokio::test]
    async fn a_decision_is_admitted_and_answered_with_its_single_result() {
        let fixture = Fixture::serving(Script::Complete);

        assert_eq!(
            fixture.frames(fixture.decision()).await,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                Frame::DecisionResult(decision_result()),
            ]
        );
        assert_eq!(
            fixture.invoker.invoked_requests(),
            [InferenceRequest::Decision(decision_request())]
        );
    }

    #[tokio::test]
    async fn a_decision_engine_rejection_passes_through_unchanged() {
        let fixture = Fixture::serving(Script::Passthrough);

        assert_eq!(
            fixture.frames(fixture.decision()).await,
            [
                Frame::Admitted(ExactTargetAdmitted {}),
                Frame::Passthrough(PassthroughResponse {
                    status: 400,
                    headers: vec![PassthroughHeader {
                        name: "content-type".to_owned(),
                        value: "application/json".to_owned(),
                    }],
                    body: b"{\"error\":\"bad\"}".to_vec(),
                }),
            ]
        );
    }

    #[tokio::test]
    async fn decision_failures_before_engine_work_keep_their_safe_to_fallback_disposition() {
        for (script, kind) in [
            (Script::RefuseConnection, ExactTargetFailureKind::Provider),
            (
                Script::UnsupportedRequestKind,
                ExactTargetFailureKind::UnsupportedRequestKind,
            ),
        ] {
            let fixture = Fixture::serving(script);

            assert_eq!(
                fixture.frames(fixture.decision()).await,
                [
                    Frame::Admitted(ExactTargetAdmitted {}),
                    failure(ExactTargetFailureDisposition::SafeToFallback, kind),
                ]
            );
        }
    }

    #[tokio::test]
    async fn an_invalid_decision_is_rejected_before_admission_without_echoing_names() {
        let fixture = Fixture::serving(Script::Complete);
        let mut request = fixture.decision();
        let Some(invoke_exact_target_request::Request::Decision(decision)) =
            request.request.as_mut()
        else {
            panic!("decision request");
        };
        decision.questions.push(decision.questions[0].clone());

        assert_eq!(
            fixture.frames(request).await,
            [Frame::Rejected(ExactTargetRejected {
                reason: ExactTargetRejectionReason::InvalidRequest as i32,
                safe_param: Some("questions".to_owned()),
                request_error_category: Some(RequestErrorCategory::InvalidField as i32),
            })]
        );
        assert!(fixture.invoker.invoked_models().is_empty());
    }

    fn rejected(reason: ExactTargetRejectionReason) -> Frame {
        Frame::Rejected(ExactTargetRejected {
            reason: reason as i32,
            ..Default::default()
        })
    }

    fn failure(disposition: ExactTargetFailureDisposition, kind: ExactTargetFailureKind) -> Frame {
        Frame::Failure(ExactTargetFailure {
            disposition: disposition as i32,
            kind: kind as i32,
            ..Default::default()
        })
    }
}

/// Worker-side exact-target fixtures shared by the worker service tests and the manager peer
/// adapter tests: an installed view assigning one deployment, its live local binding, and a
/// scripted engine that records every attempt and the cancellation it observed.
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use dotcore_catalog::CatalogModelId;
    use dotcore_proto::dotcore::{
        catalog::v1::InferenceRequestKind,
        deployment::v1::ExactPlacement,
        engine::v1::EngineKind,
        inference::v1::{
            ChatCompletionResult, ChatRequest, ChatStreamEvent, DecisionAnswer,
            DecisionContentPart, DecisionPredicate, DecisionPredicateAnswer, DecisionQuestion,
            DecisionRequest, DecisionResult, DecisionUsage, chat_stream_event::Event,
            decision_answer, decision_content_part, decision_question,
        },
        model::v1::{ModelDeploymentDeclaration, ModelDeploymentDesiredState},
        site::v1::ExecutionTarget,
    };
    use dotcore_site::{
        private_transport::AuthenticatedMachineIdentity,
        view::fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    };
    use tokio::sync::Notify;
    use tokio_stream::StreamExt;

    use super::*;
    use crate::{
        inference::{
            DecisionResponse, EngineBinding, EngineBindingIdentity, EngineInvoker,
            EngineProbeError, EngineReadiness, EngineReadinessProbe, InferenceAttempt,
            InferenceAttemptDisposition, InferenceCancellation, InferenceError, InferenceFuture,
            InferencePassthroughResponse, InferenceResponse,
        },
        model::{
            controller::serving_targets::LocalServingTargetsPublisher, domain::ModelDeploymentId,
        },
        server::private::PrivateServiceAssembly,
    };

    pub(crate) const SITE_ID: &str = "site-one";
    pub(crate) const WORKER_NODE_ID: &str = "worker-one";
    pub(crate) const DEPLOYMENT: &str = "deployment-a";
    pub(crate) const GENERATION: u64 = 3;

    pub(crate) struct Fixture {
        pub(crate) view: AcceptedNodeViewAuthority,
        pub(crate) publisher: LocalServingTargetsPublisher,
        pub(crate) service: PeerInferenceService,
        pub(crate) invoker: Arc<ScriptedInvoker>,
        pub(crate) binding_token: String,
    }

    impl Fixture {
        /// Installs a view assigning the deployment and publishes its live local binding.
        pub(crate) fn serving(script: Script) -> Self {
            let fixture = Self::installed(script);
            fixture.publisher.publish(vec![fixture.local_target()]);
            fixture
        }

        /// Installs a view assigning the deployment before any realizer publishes a binding.
        pub(crate) fn installed(script: Script) -> Self {
            let view = AcceptedNodeViewAuthority::new(
                SITE_ID,
                WORKER_NODE_ID,
                Self::node_incarnation(),
                Self::manager_session(),
                Duration::from_secs(30),
            )
            .expect("view authority");
            let projection = NodeViewProjection::new(
                vec![ExecutionTarget {
                    execution_target_id: "target-a".to_owned(),
                    site_node_id: WORKER_NODE_ID.to_owned(),
                    ..Default::default()
                }],
                vec![ModelDeploymentDeclaration {
                    model_deployment_id: DEPLOYMENT.to_owned(),
                    placement: Some(ExactPlacement {
                        site_node_id: WORKER_NODE_ID.to_owned(),
                        execution_target_id: "target-a".to_owned(),
                    }),
                    desired_state: ModelDeploymentDesiredState::Present as i32,
                    lifecycle_generation: GENERATION,
                    ..Default::default()
                }],
            )
            .expect("node view projection");
            view.install_for_test(SITE_ID, WORKER_NODE_ID, 1, &projection)
                .expect("installed view");
            let (publisher, source) = LocalServingTargetsPublisher::new();
            let invoker = Arc::new(ScriptedInvoker::new(script));
            let mut fixture = Self {
                service: PeerInferenceService::new(
                    ExactTargetAdmission::new(view.clone(), source),
                    &PrivateServiceContext::new(PrivateNodeRole::Worker, WORKER_NODE_ID),
                ),
                view,
                publisher,
                invoker,
                binding_token: String::new(),
            };
            fixture.binding_token = fixture.local_target().binding_token();
            fixture
        }

        pub(crate) fn manager_session() -> ManagerSession {
            ManagerSession::parse("a".repeat(32)).expect("manager session")
        }

        pub(crate) fn node_incarnation() -> NodeIncarnation {
            NodeIncarnation::parse("1".repeat(32)).expect("worker incarnation")
        }

        /// The node's own exact-target admission over this fixture's view and bindings.
        pub(crate) fn admission(&self) -> ExactTargetAdmission {
            ExactTargetAdmission::new(self.view.clone(), self.publisher.source())
        }

        /// Private services a worker listener mounts to serve this fixture's exact target.
        pub(crate) fn private_services(&self) -> PrivateServiceAssembly {
            let view = self.view.clone();
            let targets = self.publisher.source();
            PrivateServiceAssembly::new(move |context, routes| {
                routes.add_service(
                    PeerInferenceService::new(ExactTargetAdmission::new(view, targets), &context)
                        .into_server(),
                );
            })
        }

        pub(crate) fn local_target(&self) -> LocalServingTarget {
            let identity = EngineBindingIdentity::new(DEPLOYMENT, "realization-a", "http", 1)
                .expect("identity");
            LocalServingTarget {
                model_id: CatalogModelId("model-a".to_owned()),
                engine_kind: EngineKind::LlamaCpp as i32,
                deployment_id: ModelDeploymentId(DEPLOYMENT.to_owned()),
                lifecycle_generation: GENERATION,
                reconcile_generation: 1,
                binding_identity: identity.clone(),
                binding: EngineBinding::new(
                    identity,
                    Arc::new(ReadyProbe),
                    Arc::clone(&self.invoker) as Arc<dyn EngineInvoker>,
                ),
                image_capacity: None,
                request_kinds: std::collections::BTreeSet::from([InferenceRequestKind::Chat]),
            }
        }

        pub(crate) fn request(&self) -> InvokeExactTargetRequest {
            InvokeExactTargetRequest {
                target: Some(ExactServingTarget {
                    manager_session_id: Self::manager_session().as_str().to_owned(),
                    node_incarnation_id: Self::node_incarnation().as_str().to_owned(),
                    model_deployment_id: DEPLOYMENT.to_owned(),
                    lifecycle_generation: GENERATION,
                    binding_token: self.binding_token.clone(),
                }),
                request: Some(invoke_exact_target_request::Request::Chat(ChatRequest {
                    model_id: "route-a".to_owned(),
                    ..Default::default()
                })),
            }
        }

        /// An exact-target request carrying [`decision_request`] to this fixture's target.
        pub(crate) fn decision(&self) -> InvokeExactTargetRequest {
            InvokeExactTargetRequest {
                request: Some(invoke_exact_target_request::Request::Decision(
                    decision_request(),
                )),
                ..self.request()
            }
        }

        pub(crate) async fn call(
            &self,
            request: InvokeExactTargetRequest,
            role: Option<PrivateNodeRole>,
        ) -> Result<Response<ExactTargetFrames>, Status> {
            self.service
                .invoke_exact_target(authenticated(request, role))
                .await
        }

        pub(crate) async fn frames(&self, request: InvokeExactTargetRequest) -> Vec<Frame> {
            let mut stream = self
                .call(request, Some(PrivateNodeRole::Manager))
                .await
                .expect("authorized call")
                .into_inner();
            let mut frames = Vec::new();
            while let Some(frame) = next_frame(&mut stream).await {
                frames.push(frame);
            }
            frames
        }
    }

    #[derive(Clone)]
    pub(crate) enum Script {
        Complete,

        RefuseConnection,

        /// Refuses like [`Self::RefuseConnection`] once the test notifies the gate, so the
        /// refused attempt takes as long as the test decides.
        RefuseWhenReleased(Arc<Notify>),

        Passthrough,

        /// Ends the attempt with its own cancellation, as a node reports one the router's
        /// local state does not hold.
        CancelledBy(RequestCancellationReason),

        Stream(Vec<Event>),

        StreamThenFail(Vec<Event>),

        StreamThenAwaitCancellation(Vec<Event>),

        AwaitCancellation,

        /// The binding refuses the admitted request as invalid, as its encoder or its engine
        /// rejection classifier would.
        InvalidRequest(InvalidRequest),

        /// The engine does not serve the request's kind and refuses it before any work.
        UnsupportedRequestKind,
    }

    /// Engine invoker that records each attempt and plays one scripted outcome.
    pub(crate) struct ScriptedInvoker {
        script: Script,
        requests: Mutex<Vec<InferenceRequest>>,
        pub(crate) invoked: Notify,
        cancelled: watch::Sender<Option<RequestCancellationReason>>,
    }

    impl ScriptedInvoker {
        fn new(script: Script) -> Self {
            Self {
                script,
                requests: Mutex::default(),
                invoked: Notify::new(),
                cancelled: watch::channel(None).0,
            }
        }

        /// The chat requests the engine was invoked with, in order.
        pub(crate) fn requests(&self) -> Vec<ChatRequest> {
            self.invoked_requests()
                .into_iter()
                .filter_map(|request| match request {
                    InferenceRequest::Chat(request) => Some(request),
                    InferenceRequest::Decision(_) => None,
                })
                .collect()
        }

        /// The requests of every kind the engine was invoked with, in order.
        pub(crate) fn invoked_requests(&self) -> Vec<InferenceRequest> {
            self.requests.lock().expect("invoked requests").clone()
        }

        pub(crate) fn invoked_models(&self) -> Vec<String> {
            self.invoked_requests()
                .iter()
                .map(|request| request.model_id().to_owned())
                .collect()
        }

        fn record(&self, request: InferenceRequest) {
            self.requests
                .lock()
                .expect("invoked requests")
                .push(request);
            self.invoked.notify_one();
        }

        /// Plays a script ending in a failure, which every request kind answers alike.
        async fn failure<R>(
            &self,
            script: &Script,
            mut cancellation: InferenceCancellation,
        ) -> InferenceAttempt<R> {
            match script {
                Script::RefuseConnection => InferenceAttempt::failed(
                    InferenceAttemptDisposition::SafeToFallback,
                    InferenceError::Provider(anyhow::anyhow!("connection refused")),
                ),
                Script::RefuseWhenReleased(release) => {
                    release.notified().await;
                    InferenceAttempt::failed(
                        InferenceAttemptDisposition::SafeToFallback,
                        InferenceError::Provider(anyhow::anyhow!("connection refused")),
                    )
                }
                Script::UnsupportedRequestKind => InferenceAttempt::unsupported_request_kind(),
                Script::CancelledBy(reason) => InferenceAttempt::failed(
                    InferenceAttemptDisposition::StartedOrAmbiguous,
                    InferenceError::Cancelled(*reason),
                ),
                Script::InvalidRequest(invalid) => InferenceAttempt::failed(
                    InferenceAttemptDisposition::NonRetryable,
                    InferenceError::InvalidRequest(*invalid),
                ),
                Script::AwaitCancellation => {
                    let reason = cancellation.cancelled().await;
                    self.cancelled.send_replace(Some(reason));
                    InferenceAttempt::failed(
                        InferenceAttemptDisposition::StartedOrAmbiguous,
                        InferenceError::Cancelled(reason),
                    )
                }
                Script::Complete
                | Script::Passthrough
                | Script::Stream(_)
                | Script::StreamThenFail(_)
                | Script::StreamThenAwaitCancellation(_) => {
                    panic!("script answers only the request kind it was written for")
                }
            }
        }

        /// Waits until the engine attempt observes its cancellation and returns the reason.
        pub(crate) async fn cancellation(&self) -> RequestCancellationReason {
            let mut observed = self.cancelled.subscribe();
            let reason =
                tokio::time::timeout(Duration::from_secs(5), observed.wait_for(Option::is_some))
                    .await
                    .expect("engine observes cancellation promptly")
                    .expect("cancellation observation");
            reason.expect("observed reason")
        }
    }

    impl EngineInvoker for ScriptedInvoker {
        fn invoke<'a>(
            &'a self,
            request: ChatRequest,
            mut cancellation: InferenceCancellation,
        ) -> InferenceFuture<'a, InferenceAttempt> {
            self.record(InferenceRequest::Chat(request));
            Box::pin(async move {
                match &self.script {
                    Script::Complete => {
                        InferenceAttempt::Response(InferenceResponse::Complete(completion()))
                    }
                    Script::Passthrough => {
                        InferenceAttempt::Response(InferenceResponse::Passthrough(passthrough()))
                    }
                    Script::Stream(events) => InferenceAttempt::Response(
                        InferenceResponse::Stream(scripted_stream(events, None)),
                    ),
                    Script::StreamThenFail(events) => {
                        InferenceAttempt::Response(InferenceResponse::Stream(scripted_stream(
                            events,
                            Some(InferenceError::Provider(anyhow::anyhow!(
                                "engine stream broke"
                            ))),
                        )))
                    }
                    Script::StreamThenAwaitCancellation(events) => {
                        let (sender, receiver) = mpsc::channel(events.len() + 1);
                        for event in events {
                            sender
                                .try_send(Ok(ChatStreamEvent {
                                    event: Some(event.clone()),
                                }))
                                .expect("scripted event");
                        }
                        let cancelled = self.cancelled.clone();
                        tokio::spawn(async move {
                            let reason = cancellation.cancelled().await;
                            cancelled.send_replace(Some(reason));
                            drop(sender);
                        });
                        InferenceAttempt::Response(InferenceResponse::Stream(receiver))
                    }
                    script => self.failure(script, cancellation).await,
                }
            })
        }

        fn invoke_decision<'a>(
            &'a self,
            request: DecisionRequest,
            cancellation: InferenceCancellation,
        ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>> {
            self.record(InferenceRequest::Decision(request));
            Box::pin(async move {
                match &self.script {
                    Script::Complete => {
                        InferenceAttempt::Response(DecisionResponse::Complete(decision_result()))
                    }
                    Script::Passthrough => {
                        InferenceAttempt::Response(DecisionResponse::Passthrough(passthrough()))
                    }
                    script => self.failure(script, cancellation).await,
                }
            })
        }
    }

    struct ReadyProbe;

    impl EngineReadinessProbe for ReadyProbe {
        fn probe<'a>(
            &'a self,
            _: InferenceCancellation,
        ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
            Box::pin(std::future::ready(Ok(EngineReadiness::Ready {
                served_model_id: "model-a".to_owned(),
            })))
        }
    }

    pub(crate) fn authenticated(
        request: InvokeExactTargetRequest,
        role: Option<PrivateNodeRole>,
    ) -> Request<InvokeExactTargetRequest> {
        let mut request = Request::new(request);
        if let Some(role) = role {
            request
                .extensions_mut()
                .insert(AuthenticatedMachineIdentity {
                    site_id: SITE_ID.to_owned(),
                    node_id: "manager-one".to_owned(),
                    role,
                });
        }
        request
    }

    pub(crate) async fn next_frame(stream: &mut ExactTargetFrames) -> Option<Frame> {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("frame arrives promptly")
            .map(|frame| frame.expect("frame").frame.expect("frame payload"))
    }

    pub(crate) fn completion() -> ChatCompletionResult {
        ChatCompletionResult {
            completion_id: "completion-a".to_owned(),
            model_id: "route-a".to_owned(),
            created_at_unix_seconds: 7,
            ..Default::default()
        }
    }

    /// A valid decision request for `route-a` asking one predicate question.
    pub(crate) fn decision_request() -> DecisionRequest {
        DecisionRequest {
            model_id: "route-a".to_owned(),
            state: vec![DecisionContentPart {
                content: Some(decision_content_part::Content::TextContent(
                    "Refund please".to_owned(),
                )),
            }],
            questions: vec![DecisionQuestion {
                name: "urgent".to_owned(),
                instructions: Vec::new(),
                kind: Some(decision_question::Kind::Predicate(
                    DecisionPredicate::default(),
                )),
            }],
        }
    }

    /// The answer the scripted engine gives to [`decision_request`].
    pub(crate) fn decision_result() -> DecisionResult {
        DecisionResult {
            answers: vec![DecisionAnswer {
                answer: Some(decision_answer::Answer::Predicate(
                    DecisionPredicateAnswer {
                        true_probability: Some(0.75),
                        value: Some(true),
                        confidence: None,
                    },
                )),
            }],
            usage: Some(DecisionUsage { input_tokens: 4 }),
        }
    }

    fn passthrough() -> InferencePassthroughResponse {
        InferencePassthroughResponse {
            status: 400,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: b"{\"error\":\"bad\"}".to_vec(),
        }
    }

    /// Engine stream that yields the scripted events, then the optional failure, then ends.
    fn scripted_stream(
        events: &[Event],
        failure: Option<InferenceError>,
    ) -> mpsc::Receiver<Result<ChatStreamEvent, InferenceError>> {
        let (sender, receiver) = mpsc::channel(events.len() + 1);
        for event in events {
            sender
                .try_send(Ok(ChatStreamEvent {
                    event: Some(event.clone()),
                }))
                .expect("scripted event");
        }
        if let Some(failure) = failure {
            sender.try_send(Err(failure)).expect("scripted failure");
        }
        receiver
    }
}
