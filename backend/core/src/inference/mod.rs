//! Private engine-neutral contracts shared by model-engine integrations and request routing.
//!
//! The module owns process-local binding identity, readiness probing, one-attempt
//! invocation, cancellation, and canonical attempt outcomes. It has no dependency
//! on runtime implementations, engine implementations, controller policy, or router policy.

pub(crate) mod decision;
pub mod simulated;
use std::{
    collections::HashMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;
use dotcore_proto::dotcore::engine::v1::ResolvedEngineModelState;
use dotcore_proto::dotcore::inference::v1::{
    ChatCompletionResult, ChatRequest, ChatStreamEvent, DecisionRequest, DecisionResult,
};
use tokio::sync::{mpsc, watch};

/// Boxed future returned by one engine probe or invocation operation.
pub(crate) type InferenceFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Stream of canonical inference events from one admitted invocation.
pub(crate) type InferenceEventStream = mpsc::Receiver<Result<ChatStreamEvent, InferenceError>>;

/// Exact process-local identity for one bound engine service.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct EngineBindingIdentity {
    deployment_id: String,
    realization_id: String,
    service_name: String,
    revision: u64,
}

impl EngineBindingIdentity {
    /// Builds an identity tied to one deployment, realization, named service, and revision.
    pub(crate) fn new(
        deployment_id: impl Into<String>,
        realization_id: impl Into<String>,
        service_name: impl Into<String>,
        revision: u64,
    ) -> Result<Self, EngineBindingIdentityError> {
        let deployment_id = deployment_id.into();
        let realization_id = realization_id.into();
        let service_name = service_name.into();
        if deployment_id.is_empty() {
            return Err(EngineBindingIdentityError::EmptyDeployment);
        }
        if realization_id.is_empty() {
            return Err(EngineBindingIdentityError::EmptyRealization);
        }
        if service_name.is_empty() {
            return Err(EngineBindingIdentityError::EmptyService);
        }
        if revision == 0 {
            return Err(EngineBindingIdentityError::ZeroRevision);
        }
        Ok(Self {
            deployment_id,
            realization_id,
            service_name,
            revision,
        })
    }

    pub(crate) fn deployment_id(&self) -> &str {
        &self.deployment_id
    }

    pub(crate) fn realization_id(&self) -> &str {
        &self.realization_id
    }

    pub(crate) fn service_name(&self) -> &str {
        &self.service_name
    }

    pub(crate) fn revision(&self) -> u64 {
        self.revision
    }
}

/// Invalid engine-binding identity input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum EngineBindingIdentityError {
    #[error("engine binding requires a deployment identity")]
    EmptyDeployment,

    #[error("engine binding requires a realization identity")]
    EmptyRealization,

    #[error("engine binding requires a service name")]
    EmptyService,

    #[error("engine binding revision must be non-zero")]
    ZeroRevision,
}

/// Transport or native access resolved from one opaque runtime service reference.
pub(crate) enum EngineServiceAccess {
    Loopback { host: std::net::Ipv4Addr, port: u16 },

    Native(Arc<dyn NativeInferenceProvider>),
}

/// Input for binding an engine integration to one exact realized service.
pub(crate) struct EngineBindingRequest {
    pub identity: EngineBindingIdentity,
    pub served_model_id: String,
    pub resolved_engine_state: ResolvedEngineModelState,
    pub access: EngineServiceAccess,
}

/// Engine-specific serving integration selected independently of runtime implementation.
pub(crate) trait EngineServingIntegration: Send + Sync {
    fn bind(&self, request: EngineBindingRequest) -> Result<EngineBinding, EngineBindingError>;
}

/// Native serving implementation registered beside a runtime realization.
pub(crate) trait NativeInferenceProvider: Send + Sync {
    fn bind(
        &self,
        identity: EngineBindingIdentity,
        served_model_id: String,
    ) -> Result<EngineBinding, EngineBindingError>;
}

/// Resolves exact service identity to transport or a retained native provider.
#[derive(Clone, Default)]
pub(crate) struct EngineBindingResolver {
    bindings: Arc<RwLock<HashMap<EngineBindingIdentity, RetainedEngineBinding>>>,
}

struct RetainedEngineBinding {
    served_model_id: String,
    binding: EngineBinding,
    revocation: watch::Sender<Option<InferenceCancellationReason>>,
}

impl EngineBindingResolver {
    pub(crate) fn issue_identity(
        deployment_id: impl Into<String>,
        realization_id: impl Into<String>,
        service_name: impl Into<String>,
        revision: u64,
    ) -> Result<EngineBindingIdentity, EngineBindingIdentityError> {
        EngineBindingIdentity::new(deployment_id, realization_id, service_name, revision)
    }

    /// Removes terminal binding state after its exact service is confirmed inactive.
    pub(crate) fn retire_inactive(&self, identity: &EngineBindingIdentity) -> bool {
        let retained = self
            .bindings
            .write()
            .unwrap_or_else(|value| value.into_inner())
            .remove(identity);
        if let Some(retained) = retained {
            retained.binding.revoke();
            true
        } else {
            false
        }
    }

    /// Removes binding state after every realization for a deployment is confirmed inactive.
    pub(crate) fn retire_inactive_deployment(&self, deployment_id: &str) -> usize {
        let identities = self
            .bindings
            .read()
            .unwrap_or_else(|value| value.into_inner())
            .keys()
            .filter(|identity| identity.deployment_id() == deployment_id)
            .cloned()
            .collect::<Vec<_>>();
        let mut retired = 0;
        for identity in identities {
            retired += usize::from(self.retire_inactive(&identity));
        }
        retired
    }

    pub(crate) fn bind(
        &self,
        integration: &dyn EngineServingIntegration,
        identity: EngineBindingIdentity,
        served_model_id: String,
        access: EngineServiceAccess,
        resolved_engine_state: ResolvedEngineModelState,
    ) -> Result<EngineBinding, EngineBindingError> {
        if let Some(retained) = self
            .bindings
            .read()
            .unwrap_or_else(|value| value.into_inner())
            .get(&identity)
        {
            return retained_binding(retained, &served_model_id);
        }
        let binding = integration.bind(EngineBindingRequest {
            identity: identity.clone(),
            served_model_id: served_model_id.clone(),
            resolved_engine_state,
            access,
        })?;
        let (revocation, _) = watch::channel(None);
        let binding = EngineBinding::new(
            identity.clone(),
            Arc::new(RevocableProbe {
                inner: Arc::clone(binding.probe()),
                revocation: revocation.subscribe(),
            }),
            Arc::new(RevocableInvoker {
                inner: Arc::clone(binding.invoker()),
                revocation: revocation.subscribe(),
            }),
        )
        .with_revocation(revocation.clone());
        let mut bindings = self
            .bindings
            .write()
            .unwrap_or_else(|value| value.into_inner());
        if let Some(retained) = bindings.get(&identity) {
            return retained_binding(retained, &served_model_id);
        }
        bindings.insert(
            identity,
            RetainedEngineBinding {
                served_model_id,
                binding: binding.clone(),
                revocation,
            },
        );
        Ok(binding)
    }
}

fn retained_binding(
    retained: &RetainedEngineBinding,
    served_model_id: &str,
) -> Result<EngineBinding, EngineBindingError> {
    if retained.served_model_id != served_model_id {
        return Err(EngineBindingError::Invalid(
            "retained binding model identity changed".to_owned(),
        ));
    }
    if retained.revocation.borrow().is_some() {
        return Err(EngineBindingError::Unavailable(
            "retained binding was revoked".to_owned(),
        ));
    }
    Ok(retained.binding.clone())
}

struct RevocableProbe {
    inner: Arc<dyn EngineReadinessProbe>,
    revocation: watch::Receiver<Option<InferenceCancellationReason>>,
}

impl EngineReadinessProbe for RevocableProbe {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>> {
        if self.revocation.borrow().is_some() {
            return Box::pin(std::future::ready(Err(EngineProbeError::Transient)));
        }
        self.inner
            .probe(cancellation.with_binding_revocation(self.revocation.clone()))
    }
}

struct RevocableInvoker {
    inner: Arc<dyn EngineInvoker>,
    revocation: watch::Receiver<Option<InferenceCancellationReason>>,
}

impl EngineInvoker for RevocableInvoker {
    fn invoke<'a>(
        &'a self,
        request: ChatRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt> {
        if self.revocation.borrow().is_some() {
            return Box::pin(std::future::ready(InferenceAttempt::failed(
                InferenceAttemptDisposition::StartedOrAmbiguous,
                InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked),
            )));
        }
        self.inner.invoke(
            request,
            cancellation.with_binding_revocation(self.revocation.clone()),
        )
    }

    fn invoke_decision<'a>(
        &'a self,
        request: DecisionRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>> {
        if self.revocation.borrow().is_some() {
            return Box::pin(std::future::ready(InferenceAttempt::failed(
                InferenceAttemptDisposition::StartedOrAmbiguous,
                InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked),
            )));
        }
        self.inner.invoke_decision(
            request,
            cancellation.with_binding_revocation(self.revocation.clone()),
        )
    }
}

/// Failure to bind one realized service to its selected engine integration.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineBindingError {
    #[error("engine service binding is unavailable: {0}")]
    Unavailable(String),

    #[error("engine service binding is invalid: {0}")]
    Invalid(String),
}

/// Process-local engine capabilities tied to one exact runtime realization.
#[derive(Clone)]
pub(crate) struct EngineBinding {
    identity: EngineBindingIdentity,
    probe: Arc<dyn EngineReadinessProbe>,
    invoker: Arc<dyn EngineInvoker>,
    revocation: Option<watch::Sender<Option<InferenceCancellationReason>>>,
}

impl EngineBinding {
    pub(crate) fn new(
        identity: EngineBindingIdentity,
        probe: Arc<dyn EngineReadinessProbe>,
        invoker: Arc<dyn EngineInvoker>,
    ) -> Self {
        Self {
            identity,
            probe,
            invoker,
            revocation: None,
        }
    }

    fn with_revocation(
        mut self,
        revocation: watch::Sender<Option<InferenceCancellationReason>>,
    ) -> Self {
        self.revocation = Some(revocation);
        self
    }

    pub(crate) fn revoke(&self) -> bool {
        self.revocation.as_ref().is_some_and(|revocation| {
            revocation.send_if_modified(|reason| {
                if reason.is_some() {
                    return false;
                }
                *reason = Some(InferenceCancellationReason::RouteRevoked);
                true
            })
        })
    }

    pub(crate) fn identity(&self) -> &EngineBindingIdentity {
        &self.identity
    }

    pub(crate) fn probe(&self) -> &Arc<dyn EngineReadinessProbe> {
        &self.probe
    }

    pub(crate) fn invoker(&self) -> &Arc<dyn EngineInvoker> {
        &self.invoker
    }
}

/// Bounded engine-health evidence interpreted by the owning controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EngineReadiness {
    Loading,

    Ready { served_model_id: String },
}

/// Produces engine-health evidence without publishing product state.
pub(crate) trait EngineReadinessProbe: Send + Sync {
    fn probe<'a>(
        &'a self,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, Result<EngineReadiness, EngineProbeError>>;
}

/// Performs one inference attempt without selecting routes or retries.
pub(crate) trait EngineInvoker: Send + Sync {
    fn invoke<'a>(
        &'a self,
        request: ChatRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt>;

    /// Answers one decision request. An engine that does not serve decisions fails with
    /// [`InferenceError::UnsupportedRequestKind`] before any work, which is safe to fall back
    /// from.
    fn invoke_decision<'a>(
        &'a self,
        request: DecisionRequest,
        cancellation: InferenceCancellation,
    ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>>;
}

/// Creates an endpoint-bound invoker without exposing engine protocol to the runtime.
pub(crate) trait EngineTransportFactory: Send + Sync {
    /// Binds the engine at a loopback port; requests reach it under its served model id.
    fn bind_loopback(&self, port: u16, served_model_id: String) -> Arc<dyn EngineInvoker>;
}

/// One canonical inference request of any request kind, as it travels through route admission,
/// peer calls, and engine invocation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InferenceRequest {
    Chat(ChatRequest),

    Decision(DecisionRequest),
}

impl InferenceRequest {
    /// The inference request kind a target must serve to answer this request.
    pub(crate) fn kind(&self) -> InferenceRequestKind {
        match self {
            Self::Chat(_) => InferenceRequestKind::Chat,
            Self::Decision(_) => InferenceRequestKind::Decision,
        }
    }

    /// The inference route name the client called.
    pub(crate) fn model_id(&self) -> &str {
        match self {
            Self::Chat(request) => &request.model_id,
            Self::Decision(request) => &request.model_id,
        }
    }
}

/// One engine attempt, retaining whether router fallback remains safe; `R` is the response type
/// of the attempted request kind.
pub(crate) enum InferenceAttempt<R = InferenceResponse> {
    Response(R),

    Failed {
        disposition: InferenceAttemptDisposition,
        error: InferenceError,
    },
}

impl<R> InferenceAttempt<R> {
    pub(crate) fn failed(disposition: InferenceAttemptDisposition, error: InferenceError) -> Self {
        Self::Failed { disposition, error }
    }

    /// Refuses a request kind the engine does not serve, before any engine work.
    pub(crate) fn unsupported_request_kind() -> Self {
        Self::failed(
            InferenceAttemptDisposition::SafeToFallback,
            InferenceError::UnsupportedRequestKind,
        )
    }
}

/// Whether an unsuccessful attempt may advance to another fallback target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InferenceAttemptDisposition {
    SafeToFallback,

    StartedOrAmbiguous,

    /// The request itself is invalid, so no target may be tried; no engine work is implied.
    NonRetryable,
}

/// Canonical result of one local or upstream inference attempt.
pub(crate) enum InferenceResponse {
    Complete(ChatCompletionResult),

    Stream(InferenceEventStream),

    Passthrough(InferencePassthroughResponse),
}

/// Canonical result of one decision attempt: the engine's answers or its unsuccessful response.
pub(crate) enum DecisionResponse {
    Complete(DecisionResult),

    Passthrough(InferencePassthroughResponse),
}

/// Unchanged non-success response returned by a serving provider.
pub(crate) struct InferencePassthroughResponse {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

/// Engine invocation failure independent of transport implementation.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InferenceError {
    #[error("inference request cancelled: {0}")]
    Cancelled(InferenceCancellationReason),

    #[error("inference provider failed: {0}")]
    Provider(#[source] anyhow::Error),

    #[error("inference request exceeds the selected target capabilities")]
    UnsupportedCapability,

    #[error("inference engine does not serve the request kind")]
    UnsupportedRequestKind,

    #[error("inference request is invalid: {0}")]
    InvalidRequest(InvalidRequest),
}

/// A canonical request that no target may serve, with only a safe category and recognized field.
///
/// The field is a public request path from a fixed allowlist, such as `store` or
/// `chat_template_kwargs`, so it never echoes caller-supplied keys or values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{category} (field: {})", param.unwrap_or("unspecified"))]
pub(crate) struct InvalidRequest {
    pub(crate) category: InvalidRequestCategory,
    pub(crate) param: Option<&'static str>,
}

impl InvalidRequest {
    pub(crate) fn invalid_field(param: Option<&'static str>) -> Self {
        Self {
            category: InvalidRequestCategory::InvalidField,
            param,
        }
    }

    pub(crate) fn unsupported_feature(param: Option<&'static str>) -> Self {
        Self {
            category: InvalidRequestCategory::UnsupportedFeature,
            param,
        }
    }

    pub(crate) fn request_limit(param: Option<&'static str>) -> Self {
        Self {
            category: InvalidRequestCategory::RequestLimit,
            param,
        }
    }

    pub(crate) fn context_length() -> Self {
        Self {
            category: InvalidRequestCategory::ContextLength,
            param: None,
        }
    }
}

/// Safe category of an invalid canonical request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum InvalidRequestCategory {
    #[error("a request field is invalid")]
    InvalidField,

    #[error("the request asks for an unsupported feature")]
    UnsupportedFeature,

    #[error("the request exceeds a size limit")]
    RequestLimit,

    #[error("the request is larger than the deployment's context")]
    ContextLength,
}

/// Bounded readiness-probe failure independent of product status.
#[derive(Debug, thiserror::Error)]
pub(crate) enum EngineProbeError {
    #[error("engine readiness is transiently unavailable")]
    Transient,

    #[error("engine readiness probe failed: {0}")]
    Terminal(#[source] anyhow::Error),
}

/// Cancellation reason shared by request routing and engine providers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InferenceCancellationReason {
    ClientDisconnected,

    RequestTimedOut,

    RouteRevoked,
}

impl fmt::Display for InferenceCancellationReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::ClientDisconnected => "client cancelled the request",
            Self::RequestTimedOut => "inference request timed out",
            Self::RouteRevoked => "serving targets were revoked",
        })
    }
}

/// Cloneable cancellation observation for probes, providers, and response writers.
#[derive(Clone)]
pub(crate) struct InferenceCancellation {
    client: watch::Receiver<Option<InferenceCancellationReason>>,
    route: watch::Receiver<Option<InferenceCancellationReason>>,
    binding: Option<watch::Receiver<Option<InferenceCancellationReason>>>,
    client_closed: bool,
    route_closed: bool,
    binding_closed: bool,
}

impl InferenceCancellation {
    pub(crate) fn new(
        client: watch::Receiver<Option<InferenceCancellationReason>>,
        route: watch::Receiver<Option<InferenceCancellationReason>>,
    ) -> Self {
        Self {
            client,
            route,
            binding: None,
            client_closed: false,
            route_closed: false,
            binding_closed: false,
        }
    }

    fn with_binding_revocation(
        mut self,
        binding: watch::Receiver<Option<InferenceCancellationReason>>,
    ) -> Self {
        self.binding = Some(binding);
        self
    }

    pub(crate) fn reason(&self) -> Option<InferenceCancellationReason> {
        // Revoking a route invalidates its admitted binding regardless of client timing.
        self.route
            .borrow()
            .as_ref()
            .copied()
            .or_else(|| {
                self.binding
                    .as_ref()
                    .and_then(|binding| binding.borrow().as_ref().copied())
            })
            .or_else(|| self.client.borrow().as_ref().copied())
    }

    pub(crate) async fn cancelled(&mut self) -> InferenceCancellationReason {
        loop {
            if let Some(reason) = self.reason() {
                return reason;
            }
            if self.client_closed
                && self.route_closed
                && (self.binding.is_none() || self.binding_closed)
            {
                std::future::pending::<()>().await;
            }
            tokio::select! {
                result = self.client.changed(), if !self.client_closed => {
                    if result.is_err() {
                        self.client_closed = true;
                    }
                }
                result = self.route.changed(), if !self.route_closed => {
                    if result.is_err() {
                        self.route_closed = true;
                    }
                }
                result = async {
                    self.binding
                        .as_mut()
                        .expect("binding cancellation receiver")
                        .changed()
                        .await
                }, if self.binding.is_some() && !self.binding_closed => {
                    if result.is_err() {
                        self.binding_closed = true;
                    }
                }
            }
        }
    }
}

/// Signals cancellation while retaining route-revocation precedence.
#[derive(Clone)]
#[allow(
    dead_code,
    reason = "controller readiness creates a cancellable token while production cancellation wiring is router-owned"
)]
pub(crate) struct InferenceCancellationSignal {
    client: watch::Sender<Option<InferenceCancellationReason>>,
    route: watch::Sender<Option<InferenceCancellationReason>>,
}

#[allow(
    dead_code,
    reason = "used by cancellation contract tests and retained for cancellable controller probes"
)]
impl InferenceCancellationSignal {
    pub(crate) fn cancel(&self, reason: InferenceCancellationReason) {
        let sender = if reason == InferenceCancellationReason::RouteRevoked {
            &self.route
        } else {
            &self.client
        };
        sender.send_if_modified(|current| {
            if cancellation_priority(*current) >= cancellation_priority(Some(reason)) {
                return false;
            }
            *current = Some(reason);
            true
        });
    }
}

pub(crate) fn inference_cancellation() -> (InferenceCancellationSignal, InferenceCancellation) {
    let (client, client_receiver) = watch::channel(None);
    let (route, route_receiver) = watch::channel(None);
    (
        InferenceCancellationSignal { client, route },
        InferenceCancellation::new(client_receiver, route_receiver),
    )
}

fn cancellation_priority(reason: Option<InferenceCancellationReason>) -> u8 {
    match reason {
        None => 0,
        Some(InferenceCancellationReason::ClientDisconnected) => 1,
        Some(InferenceCancellationReason::RequestTimedOut) => 2,
        Some(InferenceCancellationReason::RouteRevoked) => 3,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_proto::dotcore::inference::v1::{ChatCompletionResult, ChatRequest};

    use super::*;

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

    struct CompleteInvoker;

    impl EngineInvoker for CompleteInvoker {
        fn invoke<'a>(
            &'a self,
            request: ChatRequest,
            _: InferenceCancellation,
        ) -> InferenceFuture<'a, InferenceAttempt> {
            Box::pin(std::future::ready(InferenceAttempt::Response(
                InferenceResponse::Complete(ChatCompletionResult {
                    completion_id: format!("completion-{}", request.model_id),
                    ..Default::default()
                }),
            )))
        }

        fn invoke_decision<'a>(
            &'a self,
            _: DecisionRequest,
            _: InferenceCancellation,
        ) -> InferenceFuture<'a, InferenceAttempt<DecisionResponse>> {
            Box::pin(std::future::ready(InferenceAttempt::Response(
                DecisionResponse::Complete(DecisionResult::default()),
            )))
        }
    }

    struct TestNativeProvider;

    impl NativeInferenceProvider for TestNativeProvider {
        fn bind(
            &self,
            identity: EngineBindingIdentity,
            _: String,
        ) -> Result<EngineBinding, EngineBindingError> {
            Ok(test_binding(identity))
        }
    }

    struct AlphaIntegration;

    impl EngineServingIntegration for AlphaIntegration {
        fn bind(&self, request: EngineBindingRequest) -> Result<EngineBinding, EngineBindingError> {
            bind_test_access(request)
        }
    }

    struct BetaIntegration;

    impl EngineServingIntegration for BetaIntegration {
        fn bind(&self, request: EngineBindingRequest) -> Result<EngineBinding, EngineBindingError> {
            bind_test_access(request)
        }
    }

    struct RecordingIntegration {
        ports: Arc<Mutex<Vec<u16>>>,
    }

    impl EngineServingIntegration for RecordingIntegration {
        fn bind(&self, request: EngineBindingRequest) -> Result<EngineBinding, EngineBindingError> {
            let EngineServiceAccess::Loopback { host, port } = request.access else {
                return Err(EngineBindingError::Invalid(
                    "recording integration requires loopback access".to_owned(),
                ));
            };
            assert_eq!(host, std::net::Ipv4Addr::LOCALHOST);
            self.ports.lock().expect("recorded ports").push(port);
            Ok(test_binding(request.identity))
        }
    }

    fn bind_test_access(
        request: EngineBindingRequest,
    ) -> Result<EngineBinding, EngineBindingError> {
        match request.access {
            EngineServiceAccess::Loopback { host, port } => {
                assert_eq!(host, std::net::Ipv4Addr::LOCALHOST);
                assert_ne!(port, 0);
                Ok(test_binding(request.identity))
            }
            EngineServiceAccess::Native(provider) => {
                provider.bind(request.identity, request.served_model_id)
            }
        }
    }

    fn test_binding(identity: EngineBindingIdentity) -> EngineBinding {
        EngineBinding::new(identity, Arc::new(ReadyProbe), Arc::new(CompleteInvoker))
    }

    #[test]
    fn two_engine_integrations_bind_one_realization_without_runtime_branching() {
        let alpha = AlphaIntegration
            .bind(EngineBindingRequest {
                identity: EngineBindingIdentity::new("deployment-a", "realization-a", "alpha", 1)
                    .expect("alpha identity"),
                served_model_id: "model-a".to_owned(),
                resolved_engine_state: ResolvedEngineModelState::default(),
                access: EngineServiceAccess::Loopback {
                    host: std::net::Ipv4Addr::LOCALHOST,
                    port: 8080,
                },
            })
            .expect("alpha binding");
        let beta = BetaIntegration
            .bind(EngineBindingRequest {
                identity: EngineBindingIdentity::new("deployment-a", "realization-a", "beta", 1)
                    .expect("beta identity"),
                served_model_id: "model-a".to_owned(),
                resolved_engine_state: ResolvedEngineModelState::default(),
                access: EngineServiceAccess::Native(Arc::new(TestNativeProvider)),
            })
            .expect("beta binding");

        assert_eq!(alpha.identity().realization_id(), "realization-a");
        assert_eq!(beta.identity().realization_id(), "realization-a");
        assert_eq!(alpha.identity().service_name(), "alpha");
        assert_eq!(beta.identity().service_name(), "beta");
    }

    #[test]
    fn one_engine_integration_binds_transport_and_native_access() {
        let bind = |access| {
            AlphaIntegration
                .bind(EngineBindingRequest {
                    identity: EngineBindingIdentity::new(
                        "deployment-a",
                        "realization-a",
                        "inference",
                        1,
                    )
                    .expect("binding identity"),
                    served_model_id: "model-a".to_owned(),
                    resolved_engine_state: ResolvedEngineModelState::default(),
                    access,
                })
                .expect("engine binding")
        };

        let transport = bind(EngineServiceAccess::Loopback {
            host: std::net::Ipv4Addr::LOCALHOST,
            port: 8080,
        });
        let native = bind(EngineServiceAccess::Native(Arc::new(TestNativeProvider)));

        assert_eq!(transport.identity(), native.identity());
    }

    #[tokio::test]
    async fn binding_keeps_identity_probe_and_invoker_separate() {
        let identity = EngineBindingIdentity::new("deployment-a", "realization-a", "inference", 7)
            .expect("valid binding identity");
        let binding = EngineBinding::new(
            identity.clone(),
            Arc::new(ReadyProbe),
            Arc::new(CompleteInvoker),
        );
        let (_cancellation_signal, cancellation) = inference_cancellation();

        assert_eq!(binding.identity(), &identity);
        assert_eq!(
            binding
                .probe()
                .probe(cancellation.clone())
                .await
                .expect("probe succeeds"),
            EngineReadiness::Ready {
                served_model_id: "model-a".to_owned(),
            }
        );
        let InferenceAttempt::Response(InferenceResponse::Complete(result)) = binding
            .invoker()
            .invoke(
                ChatRequest {
                    model_id: "model-a".to_owned(),
                    ..Default::default()
                },
                cancellation,
            )
            .await
        else {
            panic!("fake invoker must return one complete response")
        };
        assert_eq!(result.completion_id, "completion-model-a");
    }

    #[tokio::test]
    async fn cancellation_keeps_route_revocation_precedence() {
        let (signal, mut cancellation) = inference_cancellation();

        assert_eq!(cancellation.reason(), None);
        signal.cancel(InferenceCancellationReason::ClientDisconnected);
        signal.cancel(InferenceCancellationReason::RouteRevoked);

        assert_eq!(
            cancellation.cancelled().await,
            InferenceCancellationReason::RouteRevoked
        );
    }

    #[tokio::test]
    async fn resolver_reuses_then_terminally_revokes_one_exact_binding() {
        let resolver = EngineBindingResolver::default();
        let identity = EngineBindingIdentity::new("deployment-a", "realization-a", "inference", 1)
            .expect("binding identity");
        let bind = || {
            resolver.bind(
                &AlphaIntegration,
                identity.clone(),
                "model-a".to_owned(),
                EngineServiceAccess::Native(Arc::new(TestNativeProvider)),
                ResolvedEngineModelState::default(),
            )
        };
        let first = bind().expect("first binding");
        let second = bind().expect("reused binding");

        assert_eq!(first.identity(), second.identity());
        assert!(first.revoke());
        assert!(!first.revoke());
        assert!(matches!(bind(), Err(EngineBindingError::Unavailable(_))));
        let (_, cancellation) = inference_cancellation();
        assert!(matches!(
            first
                .invoker()
                .invoke(ChatRequest::default(), cancellation.clone())
                .await,
            InferenceAttempt::Failed {
                error: InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked),
                ..
            }
        ));
        assert!(matches!(
            first
                .invoker()
                .invoke_decision(DecisionRequest::default(), cancellation)
                .await,
            InferenceAttempt::Failed {
                error: InferenceError::Cancelled(InferenceCancellationReason::RouteRevoked),
                ..
            }
        ));
        assert!(resolver.retire_inactive(&identity));
    }

    #[tokio::test]
    async fn inactive_retirement_rebinds_same_identity_to_changed_endpoint() {
        let resolver = EngineBindingResolver::default();
        let identity = EngineBindingIdentity::new("deployment-a", "realization-a", "inference", 1)
            .expect("binding identity");
        let ports = Arc::new(Mutex::new(Vec::new()));
        let integration = RecordingIntegration {
            ports: ports.clone(),
        };
        let bind = |port| {
            resolver.bind(
                &integration,
                identity.clone(),
                "model-a".to_owned(),
                EngineServiceAccess::Loopback {
                    host: std::net::Ipv4Addr::LOCALHOST,
                    port,
                },
                ResolvedEngineModelState::default(),
            )
        };

        let previous = bind(8080).expect("initial endpoint binding");
        assert_eq!(resolver.retire_inactive_deployment("deployment-a"), 1);
        let resumed = bind(9090).expect("resumed endpoint binding");

        assert_eq!(*ports.lock().expect("recorded ports"), [8080, 9090]);
        assert!(
            !previous.revoke(),
            "inactive binding must already be revoked"
        );
        let (_, cancellation) = inference_cancellation();
        assert!(matches!(
            resumed
                .invoker()
                .invoke(ChatRequest::default(), cancellation)
                .await,
            InferenceAttempt::Response(InferenceResponse::Complete(_))
        ));
    }

    #[test]
    fn deployment_retirement_removes_only_bindings_confirmed_inactive() {
        let resolver = EngineBindingResolver::default();
        for (deployment, realization) in [
            ("deployment-a", "realization-1"),
            ("deployment-a", "realization-2"),
            ("deployment-b", "realization-1"),
        ] {
            let identity = EngineBindingIdentity::new(deployment, realization, "inference", 1)
                .expect("binding identity");
            resolver
                .bind(
                    &AlphaIntegration,
                    identity,
                    "model-a".to_owned(),
                    EngineServiceAccess::Native(Arc::new(TestNativeProvider)),
                    ResolvedEngineModelState::default(),
                )
                .expect("binding");
        }

        assert_eq!(resolver.retire_inactive_deployment("deployment-a"), 2);
        assert_eq!(resolver.retire_inactive_deployment("deployment-a"), 0);
        assert_eq!(resolver.retire_inactive_deployment("deployment-b"), 1);
    }

    #[test]
    fn binding_identity_rejects_empty_parts_and_zero_revision() {
        assert_eq!(
            EngineBindingIdentity::new("", "realization", "inference", 1),
            Err(EngineBindingIdentityError::EmptyDeployment)
        );
        assert_eq!(
            EngineBindingIdentity::new("deployment", "", "inference", 1),
            Err(EngineBindingIdentityError::EmptyRealization)
        );
        assert_eq!(
            EngineBindingIdentity::new("deployment", "realization", "", 1),
            Err(EngineBindingIdentityError::EmptyService)
        );
        assert_eq!(
            EngineBindingIdentity::new("deployment", "realization", "inference", 0),
            Err(EngineBindingIdentityError::ZeroRevision)
        );
    }
}
