//! Route discovery and target selection over controller-published private targets.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use dotcore_proto::dotcore::catalog::v1::InferenceRequestKind;
use dotcore_proto::dotcore::inference::v1::{
    AvailableModel, AvailableModels, FallbackGroupSelection, InferenceRoute,
};
use tokio::sync::watch;
use tracing::{debug, debug_span, field, warn};

use super::{
    adapters::peer::PeerInferenceClient,
    cancellation::{RequestCancellation, RequestCancellationReason},
    metrics::{RequestMeter, RouterMetrics},
};
use crate::{
    inference::{InferenceRequest, InvalidRequest},
    model::controller::serving_targets::{
        NodeServingTarget, ServingTarget, ServingTargetsLease, ServingTargetsSource,
        UpstreamServingTarget,
    },
    model::resolved::ResolvedModelImageInputCapacity,
    route_manager::{CompiledRouteSnapshot, RouteManager},
};

type RouteCounterKey = (String, u64, usize);
type RouteCounterValues = BTreeMap<RouteCounterKey, u64>;

#[derive(Default)]
struct RouteCounters {
    snapshot: Option<Arc<CompiledRouteSnapshot>>,
    values: RouteCounterValues,
}

/// Router core bound to all controller-published private serving targets.
#[derive(Clone)]
pub(crate) struct InferenceRouter {
    source: ServingTargetsSource,
    routes: Option<RouteManager>,
    peers: Option<PeerInferenceClient>,
    counters: Arc<Mutex<RouteCounters>>,
    metrics: Option<RouterMetrics>,
}

/// A selected private target for a later serving adapter invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SelectedInferenceTarget {
    /// One node-hosted target reached over the private peer channel.
    Peer(NodeServingTarget),

    /// One controller-published upstream target with private dispatch data.
    Upstream(UpstreamServingTarget),
}

/// One admitted private target and its deployment-scoped serving-target lease.
pub(crate) struct InferenceTargetLease {
    target: SelectedInferenceTarget,
    serving_targets: ServingTargetsLease,
    route: Arc<InferenceRoute>,
    route_snapshot: Arc<CompiledRouteSnapshot>,
    fallback_group_index: usize,
}

impl InferenceTargetLease {
    pub(crate) fn deployment_id(&self) -> &str {
        self.target.deployment_id()
    }

    pub(crate) fn target(&self) -> &SelectedInferenceTarget {
        &self.target
    }

    pub(crate) fn route(&self) -> Arc<InferenceRoute> {
        Arc::clone(&self.route)
    }

    pub(crate) fn route_snapshot(&self) -> Arc<CompiledRouteSnapshot> {
        Arc::clone(&self.route_snapshot)
    }

    pub(crate) fn fallback_group_index(&self) -> usize {
        self.fallback_group_index
    }

    pub(crate) fn into_request(
        self,
        client: watch::Receiver<Option<RequestCancellationReason>>,
    ) -> (SelectedInferenceTarget, RequestCancellation) {
        let (_, route) = self.serving_targets.into_parts();
        (self.target, RequestCancellation::new(client, route))
    }
}

impl InferenceRouter {
    pub(crate) fn new(source: ServingTargetsSource) -> Self {
        Self {
            source,
            routes: None,
            peers: None,
            counters: Arc::new(Mutex::new(RouteCounters::default())),
            metrics: None,
        }
    }

    /// Binds the router to the in-process route manager's immutable snapshots.
    pub(crate) fn with_routes(mut self, routes: RouteManager) -> Self {
        self.routes = Some(routes);
        self
    }

    /// Lets the router forward node-hosted targets over the manager's private peer client.
    pub(crate) fn with_peer_client(mut self, peers: PeerInferenceClient) -> Self {
        self.peers = Some(peers);
        self
    }

    /// Meters every request into `recorder` on `clock`, sampling in-flight requests of the
    /// routes this router is bound to. Must run inside a Tokio runtime, after binding routes.
    pub(crate) fn with_metrics(
        mut self,
        recorder: Arc<dyn dotcore_observability::MetricRecorder>,
        clock: Arc<dyn dotcore_domain::clock::Clock>,
    ) -> Self {
        self.metrics = Some(RouterMetrics::start(recorder, clock, self.routes.clone()));
        self
    }

    /// A meter for one request of `kind` whose route handling starts now; it records nothing
    /// without metrics.
    pub(crate) fn meter(&self, kind: InferenceRequestKind) -> RequestMeter {
        self.metrics.as_ref().map_or_else(
            || RequestMeter::unmetered(kind),
            |metrics| metrics.meter(kind),
        )
    }

    /// Returns the private peer client used to reach node-hosted targets, when configured.
    pub(crate) fn peer_client(&self) -> Option<&PeerInferenceClient> {
        self.peers.as_ref()
    }

    #[cfg(test)]
    pub(crate) fn route_manager_pointer(&self) -> Option<usize> {
        self.routes.as_ref().map(RouteManager::shared_pointer)
    }

    /// Lists enabled route names that have at least one eligible text target.
    pub(crate) fn available_models(&self) -> AvailableModels {
        let span = debug_span!(
            "inference_list_models",
            operation = "inference_list_models",
            model_count = field::Empty,
            outcome = field::Empty,
        );
        let targets = self.router_targets();
        let models = self
            .routes
            .as_ref()
            .map(RouteManager::snapshot)
            .map_or_else(Vec::new, |snapshot| {
                snapshot
                    .routes()
                    .iter()
                    .filter(|route| route.enabled)
                    .filter(|route| route_has_eligible_text_target(route, &targets))
                    .map(|route| AvailableModel {
                        model_id: route.name.clone(),
                    })
                    .collect()
            });
        span.record("model_count", models.len());
        span.record("outcome", "success");
        AvailableModels { models }
    }

    /// Validates the canonical request, then resolves its route and atomically admits one of
    /// the route's eligible deployment targets. An invalid request never reaches a target.
    #[cfg(test)]
    pub(crate) fn admit(
        &self,
        request: &InferenceRequest,
    ) -> Result<InferenceTargetLease, TargetSelectionError> {
        let (routes, route) = self.resolve_route(request)?;
        self.admit_route(routes, route, request)
    }

    /// Validates the canonical request and resolves the enabled route it names, with the route
    /// snapshot it was read from. A request whose kind no route member declares is invalid,
    /// whether or not any member is ready.
    pub(crate) fn resolve_route(
        &self,
        request: &InferenceRequest,
    ) -> Result<(Arc<CompiledRouteSnapshot>, Arc<InferenceRoute>), TargetSelectionError> {
        validate_request(request).map_err(TargetSelectionError::InvalidRequest)?;
        let Some(routes) = &self.routes else {
            return Err(TargetSelectionError::Unavailable);
        };
        let routes = routes.snapshot();
        let route = routes
            .routes()
            .iter()
            .find(|route| route.name == request.model_id() && route.enabled)
            .cloned()
            .ok_or(TargetSelectionError::NotFound)?;
        if !routes
            .request_kinds(&route.inference_route_id)
            .contains(&request.kind())
        {
            return Err(TargetSelectionError::RequestKindNotServed);
        }
        Ok((routes, Arc::new(route)))
    }

    /// Atomically admits one of a resolved route's eligible deployment targets, from its first
    /// fallback group on.
    pub(crate) fn admit_route(
        &self,
        routes: Arc<CompiledRouteSnapshot>,
        route: Arc<InferenceRoute>,
        request: &InferenceRequest,
    ) -> Result<InferenceTargetLease, TargetSelectionError> {
        self.admit_route_from_group(routes, route, request, 0, &BTreeSet::new())
    }

    /// Admits the next declared fallback group from an already admitted route.
    pub(crate) fn admit_fallback(
        &self,
        routes: Arc<CompiledRouteSnapshot>,
        route: Arc<InferenceRoute>,
        request: &InferenceRequest,
        first_group_index: usize,
        attempted_deployments: &BTreeSet<String>,
    ) -> Result<InferenceTargetLease, TargetSelectionError> {
        self.admit_route_from_group(
            routes,
            route,
            request,
            first_group_index,
            attempted_deployments,
        )
    }

    fn admit_route_from_group(
        &self,
        routes: Arc<CompiledRouteSnapshot>,
        route: Arc<InferenceRoute>,
        request: &InferenceRequest,
        first_group_index: usize,
        attempted_deployments: &BTreeSet<String>,
    ) -> Result<InferenceTargetLease, TargetSelectionError> {
        let span = debug_span!(
            "inference_route_admit",
            operation = "inference_route_admit",
            route_name = %route.name,
            model_id = %request.model_id(),
            deployment_id = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let admitted = self.source.admit(|targets| {
            let targets = targets
                .iter()
                .cloned()
                .map(router_target)
                .collect::<Vec<_>>();
            let (target, fallback_group_index) = {
                let mut counters = self
                    .counters
                    .lock()
                    .unwrap_or_else(|value| value.into_inner());
                synchronize_counters(&mut counters, &routes);
                select_route_target(
                    &route,
                    &targets,
                    request,
                    &mut counters.values,
                    first_group_index,
                    attempted_deployments,
                )?
            };
            Ok((target.lease_target, (target.target, fallback_group_index)))
        });
        match &admitted {
            Ok(Some(((target, fallback_group_index), _))) => {
                span.record("deployment_id", target.deployment_id());
                span.record("outcome", "success");
                debug!(
                    parent: &span,
                    event_kind = "inference_target_selected",
                    route_name = %route.name,
                    deployment_id = target.deployment_id(),
                    fallback_group_index,
                    "inference target selected",
                );
            }
            Ok(None) | Err(TargetSelectionError::Unavailable) => {
                debug!(
                    parent: &span,
                    event_kind = "inference_selection_unavailable",
                    route_name = %route.name,
                    "inference route has no eligible target",
                );
                span.record("outcome", "unavailable");
                span.record("error_kind", "target_unavailable");
                warn!(
                    parent: &span,
                    event_kind = "inference_target_unavailable",
                    route_name = %route.name,
                    error_kind = "target_unavailable",
                    "inference target unavailable",
                );
            }
            Err(TargetSelectionError::InvalidImage(cause)) => {
                span.record("outcome", "rejected");
                span.record("error_kind", "invalid_image");
                warn!(
                    parent: &span,
                    event_kind = "inference_target_rejected",
                    route_name = %route.name,
                    error_kind = "invalid_image",
                    reason = %cause,
                    "inference target rejected request image",
                );
            }
            Err(
                TargetSelectionError::NotFound
                | TargetSelectionError::InvalidRequest(_)
                | TargetSelectionError::RequestKindNotServed,
            ) => {
                unreachable!(
                    "an admitted route is enabled, serves the kind and validated the request"
                )
            }
        }
        let ((target, fallback_group_index), serving_targets) =
            admitted?.ok_or(TargetSelectionError::Unavailable)?;
        Ok(InferenceTargetLease {
            target,
            serving_targets,
            route,
            route_snapshot: routes,
            fallback_group_index,
        })
    }

    fn router_targets(&self) -> Vec<RouterTarget> {
        self.source
            .snapshot()
            .targets
            .into_iter()
            .map(router_target)
            .collect()
    }
}

/// Validates the platform-owned shape of a canonical request of any kind, as every entry point
/// does before any target sees it.
pub(crate) fn validate_request(request: &InferenceRequest) -> Result<(), InvalidRequest> {
    match request {
        InferenceRequest::Chat(request) => super::request_controls::validate(request),
        InferenceRequest::Decision(request) => super::decision_validation::validate(request),
    }
}

impl SelectedInferenceTarget {
    pub(crate) fn deployment_id(&self) -> &str {
        match self {
            Self::Peer(target) => &target.deployment_id.0,
            Self::Upstream(target) => &target.deployment_id.0,
        }
    }
}

/// A request cannot select a route or exactly one private READY target.
#[derive(Debug, thiserror::Error)]
pub(crate) enum TargetSelectionError {
    #[error("the requested route was not found")]
    NotFound,

    #[error("no eligible private serving target is available")]
    Unavailable,

    #[error("image input is invalid for the selected model: {0}")]
    InvalidImage(#[source] super::images::ImageValidationError),

    #[error("inference request is invalid: {0}")]
    InvalidRequest(crate::inference::InvalidRequest),

    #[error("no member of the requested route serves the request kind")]
    RequestKindNotServed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RouterTarget {
    deployment_id: String,
    reconcile_generation: u64,
    target: SelectedInferenceTarget,
    lease_target: ServingTarget,
    image_admission: ImageAdmission,
    request_kinds: BTreeSet<InferenceRequestKind>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ImageAdmission {
    /// Manager-side admission against the capacity resolved from the declaration snapshot.
    Resolved(Option<ResolvedModelImageInputCapacity>),

    Upstream {
        supports_image: bool,
    },
}

fn router_target(target: ServingTarget) -> RouterTarget {
    let request_kinds = target.request_kinds();
    match target {
        ServingTarget::Node(target) => RouterTarget {
            deployment_id: target.deployment_id.0.clone(),
            reconcile_generation: target.lifecycle_generation,
            image_admission: ImageAdmission::Resolved(target.image_capacity.clone()),
            request_kinds,
            lease_target: ServingTarget::Node(target.clone()),
            target: SelectedInferenceTarget::Peer(target),
        },
        ServingTarget::Upstream(target) => RouterTarget {
            deployment_id: target.deployment_id.0.clone(),
            reconcile_generation: target.reconcile_generation,
            image_admission: ImageAdmission::Upstream {
                supports_image: target.supports_image,
            },
            request_kinds,
            lease_target: ServingTarget::Upstream(target.clone()),
            target: SelectedInferenceTarget::Upstream(target),
        },
    }
}

fn route_has_eligible_text_target(route: &InferenceRoute, targets: &[RouterTarget]) -> bool {
    route.fallback_groups.iter().any(|group| {
        group.model_deployment_ids.iter().any(|deployment_id| {
            unique_ready_target(targets, deployment_id)
                .is_some_and(|target| target.reconcile_generation > 0)
        })
    })
}

fn synchronize_counters(counters: &mut RouteCounters, snapshot: &Arc<CompiledRouteSnapshot>) {
    if counters
        .snapshot
        .as_ref()
        .is_none_or(|current| !Arc::ptr_eq(current, snapshot))
    {
        counters.values.clear();
        counters.snapshot = Some(Arc::clone(snapshot));
    }
}

fn select_route_target(
    route: &InferenceRoute,
    targets: &[RouterTarget],
    request: &InferenceRequest,
    counters: &mut RouteCounterValues,
    first_group_index: usize,
    attempted_deployments: &BTreeSet<String>,
) -> Result<(RouterTarget, usize), TargetSelectionError> {
    let requires_image = request_requires_image(request);
    let kind = request.kind();
    for (group_index, group) in route
        .fallback_groups
        .iter()
        .enumerate()
        .skip(first_group_index)
    {
        let members = group
            .model_deployment_ids
            .iter()
            .filter_map(|deployment_id| {
                let target = unique_ready_target(targets, deployment_id)?;
                (target.reconcile_generation > 0
                    && !attempted_deployments.contains(deployment_id)
                    && target.request_kinds.contains(&kind)
                    && (!requires_image || target_supports_images(target)))
                .then_some(target)
            })
            .collect::<Vec<_>>();
        let Some(target) =
            select_group_member(route, group_index, group.selection, members, counters)
        else {
            continue;
        };
        validate_target_image_admission(request, target)?;
        return Ok((target.clone(), group_index));
    }
    Err(TargetSelectionError::Unavailable)
}

fn target_supports_images(target: &RouterTarget) -> bool {
    match &target.image_admission {
        ImageAdmission::Resolved(capacity) => capacity.is_some(),
        ImageAdmission::Upstream { supports_image } => *supports_image,
    }
}

fn validate_target_image_admission(
    request: &InferenceRequest,
    target: &RouterTarget,
) -> Result<(), TargetSelectionError> {
    // Decision requests carry no image input.
    let InferenceRequest::Chat(request) = request else {
        return Ok(());
    };
    match &target.image_admission {
        ImageAdmission::Resolved(capacity) => {
            super::images::validate_image_input_capacity(request, capacity.as_ref())
                .map_err(TargetSelectionError::InvalidImage)
        }
        ImageAdmission::Upstream {
            supports_image: false,
        } => super::images::validate_image_input_capacity(request, None)
            .map_err(TargetSelectionError::InvalidImage),
        ImageAdmission::Upstream {
            supports_image: true,
        } => super::images::validate_upstream_image_input_capacity(request)
            .map_err(TargetSelectionError::InvalidImage),
    }
}

fn select_group_member<'a>(
    route: &InferenceRoute,
    group_index: usize,
    selection: i32,
    members: Vec<&'a RouterTarget>,
    counters: &mut RouteCounterValues,
) -> Option<&'a RouterTarget> {
    let selection = FallbackGroupSelection::try_from(selection).ok()?;
    match selection {
        FallbackGroupSelection::Single => members.into_iter().next(),
        FallbackGroupSelection::RoundRobin => {
            let counter = counters
                .entry((
                    route.inference_route_id.clone(),
                    route.generation,
                    group_index,
                ))
                .or_insert(0);
            let index = usize::try_from(*counter).ok()? % members.len();
            *counter = counter
                .checked_add(1)
                .expect("round-robin counter exhausted");
            members.into_iter().nth(index)
        }
        FallbackGroupSelection::Unspecified => None,
    }
}

fn unique_ready_target<'a>(
    targets: &'a [RouterTarget],
    deployment_id: &str,
) -> Option<&'a RouterTarget> {
    let mut targets = targets
        .iter()
        .filter(|target| target.deployment_id == deployment_id);
    let target = targets.next()?;
    targets.next().is_none().then_some(target)
}

fn request_requires_image(request: &InferenceRequest) -> bool {
    let InferenceRequest::Chat(request) = request else {
        return false;
    };
    request.messages.iter().any(|message| {
        message.content_parts.iter().any(|part| {
            matches!(
                part.content,
                Some(
                    dotcore_proto::dotcore::inference::v1::chat_content_part::Content::ImageContent(
                        _
                    )
                )
            )
        })
    })
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::inference::v1::{
        ChatContentPart, ChatImageContent, ChatImageMediaType, ChatMessage, ChatRequest,
        DecisionRequest, FallbackGroup, chat_content_part,
    };
    use dotcore_site::{private_transport::PrivateNodeRole, worker_channels::WorkerChannels};

    use super::*;
    use crate::{
        model::{
            controller::serving_targets::{
                NodeDispatch, ServingTarget, ServingTargetsPublisher, node_target_for_test,
                upstream_target_for_test,
            },
            domain::ModelDeploymentId,
        },
        test_support::private_tls::TestAuthority,
    };

    #[test]
    fn unavailable_image_admission_emits_a_bounded_router_event() {
        use dotcore_domain::test_support::tracing::{Record, capture_records};

        let (publisher, source) = ServingTargetsPublisher::new();
        publisher.replace_node_targets(vec![node_target_for_test("deployment", &"b".repeat(64))]);
        let router = InferenceRouter::new(source);
        let route = Arc::new(route(
            "route",
            vec![group(FallbackGroupSelection::Single, &["deployment"])],
        ));
        let snapshot = Arc::new(CompiledRouteSnapshot::new(vec![(*route).clone()], &[]));

        let (result, records, _) = capture_records(|| {
            router.admit_route_from_group(
                snapshot,
                route,
                &image_request("route", 1),
                0,
                &BTreeSet::new(),
            )
        });

        assert!(matches!(result, Err(TargetSelectionError::Unavailable)));
        assert!(records.iter().any(|record| matches!(
            record,
            Record::Event { level, fields }
                if level == "WARN"
                    && fields.get("event_kind") == Some(&"\"inference_target_unavailable\"".to_owned())
                    && fields.get("route_name") == Some(&"route".to_owned())
                    && fields.get("error_kind") == Some(&"\"target_unavailable\"".to_owned())
        )));
    }

    #[tokio::test]
    async fn unreachable_peer_target_is_refused_before_dispatch_and_falls_back_to_the_next_group() {
        let reserved = dotcore_domain::test_support::ports::reserve_port();
        let unreachable = std::net::SocketAddr::from(([127, 0, 0, 1], reserved.port()));
        let peer = NodeServingTarget::new(
            ModelDeploymentId("peer".to_owned()),
            1,
            "worker-one".to_owned(),
            dotcore_catalog::CatalogModelId("model-a".to_owned()),
            None,
            BTreeSet::from([InferenceRequestKind::Chat]),
            NodeDispatch {
                transport: crate::model::controller::serving_targets::NodeTransport::Remote {
                    address: unreachable.into(),
                },
                ..node_target_for_test("peer", &"b".repeat(64))
                    .dispatch()
                    .clone()
            },
        );
        let (publisher, source) = ServingTargetsPublisher::new();
        publisher.replace_node_targets(vec![
            peer,
            node_target_for_test("fallback", &"b".repeat(64)),
        ]);
        let router = InferenceRouter::new(source).with_peer_client(peer_client());
        let route = Arc::new(route(
            "route",
            vec![
                group(FallbackGroupSelection::Single, &["peer"]),
                group(FallbackGroupSelection::Single, &["fallback"]),
            ],
        ));
        let snapshot = Arc::new(CompiledRouteSnapshot::new(vec![(*route).clone()], &[]));
        let request = request("route");

        let lease = router
            .admit_route_from_group(
                Arc::clone(&snapshot),
                Arc::clone(&route),
                &request,
                0,
                &BTreeSet::new(),
            )
            .expect("peer target admitted");
        assert_eq!(lease.deployment_id(), "peer");
        let (_, client) = watch::channel(None);
        let (target, cancellation) = lease.into_request(client);
        let attempt = crate::router::adapters::invoke(
            target,
            request.clone(),
            cancellation,
            router.peer_client(),
        )
        .await;

        assert!(attempt.allows_fallback());
        let Err(error) = attempt.into_result() else {
            panic!("an unreachable worker must be refused");
        };
        assert!(matches!(
            &error,
            crate::router::adapters::AdapterError::PreDispatchUnavailable(source)
                if source.to_string() == "worker refused or was unreachable before dispatch"
        ));
        let fallback = router
            .admit_fallback(
                snapshot,
                route,
                &request,
                1,
                &BTreeSet::from(["peer".to_owned()]),
            )
            .expect("next group admitted");
        assert_eq!(fallback.deployment_id(), "fallback");
    }

    #[test]
    fn routes_admit_only_members_serving_the_request_kind() {
        let routing = KindRouting::new();
        let ready = |chat: bool, decision: bool| {
            let mut targets = Vec::new();
            if chat {
                targets.push(node_target_for_test("chat-model", &"b".repeat(64)));
            }
            if decision {
                targets.push(
                    node_target_for_test("decision-model", &"b".repeat(64))
                        .with_request_kinds(BTreeSet::from([InferenceRequestKind::Decision])),
                );
            }
            routing.publisher.replace_node_targets(targets);
        };
        let admitted = |request: &InferenceRequest| {
            routing
                .router
                .admit(request)
                .map(|lease| lease.deployment_id().to_owned())
        };

        ready(true, true);
        assert_eq!(
            admitted(&request("chat-only")).ok(),
            Some("chat-model".to_owned())
        );
        assert_eq!(
            admitted(&decision("decision-only")).ok(),
            Some("decision-model".to_owned())
        );
        assert_eq!(
            admitted(&request("mixed")).ok(),
            Some("chat-model".to_owned())
        );
        assert_eq!(
            admitted(&decision("mixed")).ok(),
            Some("decision-model".to_owned())
        );
        // A kind no member declares is invalid whatever is ready.
        assert!(matches!(
            admitted(&decision("chat-only")),
            Err(TargetSelectionError::RequestKindNotServed)
        ));
        assert!(matches!(
            admitted(&request("decision-only")),
            Err(TargetSelectionError::RequestKindNotServed)
        ));

        // A declared kind whose serving members are not ready is unavailable.
        ready(true, false);
        for route in ["decision-only", "mixed"] {
            assert!(
                matches!(
                    admitted(&decision(route)),
                    Err(TargetSelectionError::Unavailable)
                ),
                "{route}"
            );
        }
        ready(false, false);
        assert!(matches!(
            admitted(&decision("chat-only")),
            Err(TargetSelectionError::RequestKindNotServed)
        ));
    }

    #[test]
    fn upstream_targets_are_never_admitted_for_decisions() {
        let routing = KindRouting::new();
        // The decision member is served by an upstream target, which serves chat only.
        routing
            .publisher
            .publish(vec![ServingTarget::Upstream(upstream_target_for_test(
                "decision-model",
            ))]);

        assert!(matches!(
            routing.router.admit(&decision("decision-only")),
            Err(TargetSelectionError::Unavailable)
        ));
    }

    #[tokio::test]
    async fn a_decision_refused_before_engine_work_falls_back_to_the_next_group() {
        use crate::router::peer_service::test_support::{Fixture, GENERATION, Script};

        let decision_kinds = || BTreeSet::from([InferenceRequestKind::Decision]);
        for script in [Script::UnsupportedRequestKind, Script::RefuseConnection] {
            let node = Fixture::serving(script);
            let first = NodeServingTarget::new(
                ModelDeploymentId(crate::router::peer_service::test_support::DEPLOYMENT.to_owned()),
                GENERATION,
                "worker-one".to_owned(),
                dotcore_catalog::CatalogModelId("model-a".to_owned()),
                None,
                decision_kinds(),
                NodeDispatch {
                    transport: crate::model::controller::serving_targets::NodeTransport::InProcess,
                    manager_session: Fixture::manager_session(),
                    node_incarnation: Fixture::node_incarnation(),
                    binding_token: node.binding_token.clone(),
                },
            );
            let (publisher, source) = ServingTargetsPublisher::new();
            publisher.replace_node_targets(vec![
                first,
                node_target_for_test("fallback", &"b".repeat(64))
                    .with_request_kinds(decision_kinds()),
            ]);
            let router = InferenceRouter::new(source)
                .with_peer_client(PeerInferenceClient::new(None, node.admission()));
            let route = Arc::new(route(
                "route",
                vec![
                    group(
                        FallbackGroupSelection::Single,
                        &[crate::router::peer_service::test_support::DEPLOYMENT],
                    ),
                    group(FallbackGroupSelection::Single, &["fallback"]),
                ],
            ));
            let snapshot = Arc::new(CompiledRouteSnapshot::new(vec![(*route).clone()], &[]));
            let request = decision("route");

            let lease = router
                .admit_route(Arc::clone(&snapshot), Arc::clone(&route), &request)
                .expect("first decision member admitted");
            let first_deployment = lease.deployment_id().to_owned();
            let (_, client) = watch::channel(None);
            let (target, cancellation) = lease.into_request(client);
            let attempt = crate::router::adapters::invoke(
                target,
                request.clone(),
                cancellation,
                router.peer_client(),
            )
            .await;

            assert_eq!(
                first_deployment,
                crate::router::peer_service::test_support::DEPLOYMENT
            );
            assert!(attempt.allows_fallback());
            assert_eq!(
                node.invoker.invoked_requests(),
                std::slice::from_ref(&request)
            );
            let fallback = router
                .admit_fallback(
                    snapshot,
                    route,
                    &request,
                    1,
                    &BTreeSet::from([first_deployment]),
                )
                .expect("next group admitted");
            assert_eq!(fallback.deployment_id(), "fallback");
        }
    }

    #[test]
    fn an_invalid_canonical_request_is_refused_before_any_route_or_target() {
        let (_publisher, source) = ServingTargetsPublisher::new();
        let router = InferenceRouter::new(source);
        let invalid_chat = InferenceRequest::Chat(ChatRequest {
            model_id: "route".to_owned(),
            request_extensions_json: Some(r#"{"model":"other-route"}"#.to_owned()),
            ..Default::default()
        });
        let invalid_decision = InferenceRequest::Decision(DecisionRequest {
            model_id: "route".to_owned(),
            ..Default::default()
        });

        assert!(matches!(
            router.admit(&invalid_chat),
            Err(TargetSelectionError::InvalidRequest(error))
                if error == InvalidRequest::invalid_field(Some("model"))
        ));
        assert!(matches!(
            router.admit(&invalid_decision),
            Err(TargetSelectionError::InvalidRequest(error))
                if error == InvalidRequest::invalid_field(Some("state"))
        ));
        assert!(matches!(
            router.admit(&request("route")),
            Err(TargetSelectionError::Unavailable)
        ));
    }

    #[test]
    fn ordered_fallback_skips_an_ineligible_group_and_selects_the_next_group() {
        let route = route(
            "fallback-route",
            vec![
                group(FallbackGroupSelection::Single, &["not-ready"]),
                group(FallbackGroupSelection::Single, &["ready"]),
            ],
        );
        let mut counters = BTreeMap::new();

        let (target, group) = select_route_target(
            &route,
            &[target("ready")],
            &request("fallback-route"),
            &mut counters,
            0,
            &BTreeSet::new(),
        )
        .expect("ready target");
        assert_eq!(target.deployment_id, "ready");
        assert_eq!(group, 1);
    }

    #[test]
    fn round_robin_uses_declared_member_order_with_a_route_generation_scoped_counter() {
        let route = route(
            "balanced-route",
            vec![group(
                FallbackGroupSelection::RoundRobin,
                &["first", "second"],
            )],
        );
        let targets = [target("first"), target("second")];
        let mut counters = BTreeMap::new();

        let selected = (0..3)
            .map(|_| {
                select_route_target(
                    &route,
                    &targets,
                    &request("balanced-route"),
                    &mut counters,
                    0,
                    &BTreeSet::new(),
                )
                .expect("round-robin route admission")
                .0
                .deployment_id
            })
            .collect::<Vec<_>>();

        assert_eq!(selected, ["first", "second", "first"]);
    }

    #[test]
    fn a_route_snapshot_change_discards_round_robin_counters_before_a_deleted_route_is_recreated() {
        let route = route(
            "balanced-route",
            vec![group(
                FallbackGroupSelection::RoundRobin,
                &["first", "second"],
            )],
        );
        let targets = [target("first"), target("second")];
        let mut counters = RouteCounters::default();
        let initial = Arc::new(CompiledRouteSnapshot::new(vec![route.clone()], &[]));

        synchronize_counters(&mut counters, &initial);
        let first = select_route_target(
            &route,
            &targets,
            &request("balanced-route"),
            &mut counters.values,
            0,
            &BTreeSet::new(),
        )
        .expect("initial round-robin admission");
        assert_eq!(first.0.deployment_id, "first");

        let deleted = Arc::new(CompiledRouteSnapshot::default());
        synchronize_counters(&mut counters, &deleted);
        assert!(counters.values.is_empty());

        let recreated = Arc::new(CompiledRouteSnapshot::new(vec![route.clone()], &[]));
        synchronize_counters(&mut counters, &recreated);
        let first_after_recreation = select_route_target(
            &route,
            &targets,
            &request("balanced-route"),
            &mut counters.values,
            0,
            &BTreeSet::new(),
        )
        .expect("recreated round-robin admission");

        assert_eq!(first_after_recreation.0.deployment_id, "first");
    }

    #[test]
    fn a_safe_retry_cannot_reselect_an_attempted_deployment() {
        let route = route(
            "fallback-route",
            vec![
                group(FallbackGroupSelection::Single, &["first"]),
                group(FallbackGroupSelection::Single, &["second"]),
            ],
        );
        let mut counters = BTreeMap::new();
        let attempted = BTreeSet::from(["first".to_owned()]);

        let (target, group) = select_route_target(
            &route,
            &[target("first"), target("second")],
            &request("fallback-route"),
            &mut counters,
            1,
            &attempted,
        )
        .expect("second fallback group");
        assert_eq!(target.deployment_id, "second");
        assert_eq!(group, 1);
    }

    #[test]
    fn route_membership_selects_an_upstream_target_by_deployment_id() {
        let route = route(
            "dotcore-route",
            vec![group(
                FallbackGroupSelection::Single,
                &["upstream-deployment"],
            )],
        );
        let upstream = upstream_target_for_test("upstream-deployment");
        let (target, _) = select_route_target(
            &route,
            &[router_target(ServingTarget::Upstream(upstream.clone()))],
            &request("dotcore-route"),
            &mut BTreeMap::new(),
            0,
            &BTreeSet::new(),
        )
        .expect("route member target");

        assert_eq!(target.deployment_id, "upstream-deployment");
        assert!(
            matches!(target.target, SelectedInferenceTarget::Upstream(selected) if selected == upstream)
        );
    }

    #[test]
    fn upstream_image_admission_uses_the_selected_target_capability() {
        let route = route(
            "route",
            vec![group(
                FallbackGroupSelection::Single,
                &["upstream-deployment"],
            )],
        );
        let unsupported = upstream_target_for_test("upstream-deployment");

        assert!(matches!(
            select_route_target(
                &route,
                &[router_target(ServingTarget::Upstream(unsupported))],
                &image_request("route", 1),
                &mut BTreeMap::new(),
                0,
                &BTreeSet::new(),
            ),
            Err(TargetSelectionError::Unavailable)
        ));

        let mut supported = upstream_target_for_test("upstream-deployment");
        supported.supports_image = true;
        assert!(matches!(
            select_route_target(
                &route,
                &[router_target(ServingTarget::Upstream(supported))],
                &image_request("route", 1),
                &mut BTreeMap::new(),
                0,
                &BTreeSet::new(),
            ),
            Err(TargetSelectionError::InvalidImage(_))
        ));
    }

    #[test]
    fn missing_duplicate_or_stale_route_members_fail_closed() {
        let route = route(
            "route",
            vec![group(FallbackGroupSelection::Single, &["deployment"])],
        );
        let mut stale = target("deployment");
        stale.reconcile_generation = 0;

        for targets in [
            vec![],
            vec![target("deployment"), target("deployment")],
            vec![stale],
        ] {
            assert!(matches!(
                select_route_target(
                    &route,
                    &targets,
                    &request("route"),
                    &mut BTreeMap::new(),
                    0,
                    &BTreeSet::new(),
                ),
                Err(TargetSelectionError::Unavailable)
            ));
        }
    }

    #[test]
    fn routes_only_discover_eligible_members() {
        let route = route(
            "route",
            vec![group(FallbackGroupSelection::Single, &["deployment"])],
        );
        assert!(route_has_eligible_text_target(
            &route,
            &[target("deployment")]
        ));
        assert!(!route_has_eligible_text_target(&route, &[target("other")]));
    }

    fn peer_client() -> PeerInferenceClient {
        let authority = TestAuthority::new();
        let (worker_certificate, _) = authority.issue("site-one", "worker-one", true);
        PeerInferenceClient::new(
            Some(WorkerChannels::new(&authority.private_configuration(
                "site-one",
                "manager-one",
                PrivateNodeRole::Manager,
                authority.issue("site-one", "manager-one", false),
                [(
                    "worker-one",
                    PrivateNodeRole::Worker,
                    worker_certificate.as_slice(),
                )],
            ))),
            crate::router::peer_service::test_support::Fixture::installed(
                crate::router::peer_service::test_support::Script::Complete,
            )
            .admission(),
        )
    }

    /// A router over a site declaring a chat model and a decision model, with a chat-only, a
    /// decision-only, and a mixed route; no target is ready until the test publishes one.
    struct KindRouting {
        router: InferenceRouter,
        publisher: ServingTargetsPublisher,
        _directory: tempfile::TempDir,
    }

    impl KindRouting {
        fn new() -> Self {
            use dotcore_proto::dotcore::{
                inference::v1::CreateRouteRequest, model::v1::model_deployment_snapshot::Source,
            };

            let mut site = crate::test_support::routed_site(&["chat-model", "decision-model"]);
            let Some(Source::Local(local)) = site.model_deployments[1]
                .snapshot
                .as_mut()
                .and_then(|snapshot| snapshot.source.as_mut())
            else {
                panic!("local fixture snapshot");
            };
            local.request_kinds = vec![InferenceRequestKind::Decision as i32];
            let directory = crate::test_support::canonical_tempdir().expect("site directory");
            let path = directory.path().join("site.yaml");
            std::fs::write(
                &path,
                dotcore_proto::yaml::to_string(&site).expect("site YAML"),
            )
            .expect("site fixture");
            let site = dotcore_site::repository::SiteRepository::load(
                &path,
                crate::test_support::site_declaration_rules(),
            )
            .expect("site");
            let (publisher, source) = ServingTargetsPublisher::new();
            let routes = RouteManager::new(
                crate::route_manager::RouteDeclarationRepository::new(site),
                crate::route_manager::RouteAvailabilityTargetsSource::new(source.clone()),
            )
            .expect("route manager");
            for (name, groups) in [
                ("chat-only", vec![vec!["chat-model"]]),
                ("decision-only", vec![vec!["decision-model"]]),
                ("mixed", vec![vec!["chat-model"], vec!["decision-model"]]),
            ] {
                routes
                    .create(CreateRouteRequest {
                        name: name.to_owned(),
                        enabled: true,
                        fallback_groups: groups
                            .iter()
                            .map(|members| group(FallbackGroupSelection::Single, members))
                            .collect(),
                    })
                    .expect("created route");
            }
            Self {
                router: InferenceRouter::new(source).with_routes(routes),
                publisher,
                _directory: directory,
            }
        }
    }

    fn route(name: &str, fallback_groups: Vec<FallbackGroup>) -> InferenceRoute {
        InferenceRoute {
            inference_route_id: format!("{name}-id"),
            name: name.to_owned(),
            enabled: true,
            fallback_groups,
            generation: 1,
        }
    }

    fn group(selection: FallbackGroupSelection, members: &[&str]) -> FallbackGroup {
        FallbackGroup {
            selection: selection as i32,
            model_deployment_ids: members.iter().map(ToString::to_string).collect(),
        }
    }

    fn target(deployment_id: &str) -> RouterTarget {
        router_target(ServingTarget::Node(node_target_for_test(
            deployment_id,
            &"b".repeat(64),
        )))
    }

    fn request(model_id: &str) -> InferenceRequest {
        InferenceRequest::Chat(ChatRequest {
            model_id: model_id.to_owned(),
            ..Default::default()
        })
    }

    fn decision(model_id: &str) -> InferenceRequest {
        InferenceRequest::Decision(DecisionRequest {
            model_id: model_id.to_owned(),
            ..crate::router::peer_service::test_support::decision_request()
        })
    }

    fn image_request(model_id: &str, count: usize) -> InferenceRequest {
        InferenceRequest::Chat(ChatRequest {
            model_id: model_id.to_owned(),
            messages: vec![ChatMessage {
                content_parts: (0..count)
                    .map(|_| ChatContentPart {
                        content: Some(chat_content_part::Content::ImageContent(ChatImageContent {
                            media_type: ChatImageMediaType::Png as i32,
                            encoded_data: Vec::new(),
                        })),
                    })
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}
