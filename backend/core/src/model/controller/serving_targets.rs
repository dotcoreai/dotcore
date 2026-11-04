//! Private READY serving-target views. The inference controller publishes the node-hosted and
//! upstream targets its router selects from, and each node publishes its own local runtime
//! targets, which only its status reporting and exact-target serving read.
//!
//! Each view carries one target kind, so the router can never dispatch a local runtime binding
//! without the hosting node's exact-target admission. Every target holds a per-deployment lease
//! that is revoked only when that deployment's exact target changes or disappears.

use std::fmt;
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, RwLock},
};

use dotcore_catalog::CatalogModelId;
#[cfg(test)]
use dotcore_proto::dotcore::engine::v1::EngineKind;
use dotcore_proto::dotcore::{
    catalog::v1::{InferenceRequestKind, UpstreamModelBoundary, UpstreamWireProtocol},
    model::v1::{
        ModelDeploymentDeclaration, UpstreamModelDeploymentSnapshot,
        UpstreamModelDesiredConfiguration, model_deployment_snapshot::Source,
    },
    site::v1::ExecutionTarget,
};
use dotcore_runtime::ObservedWorkload;
#[cfg(test)]
use dotcore_runtime::{LoopbackEndpoint, WorkloadServiceBinding};
use dotcore_site::{
    view::fencing::{ManagerSession, NodeIncarnation},
    worker_channels::WorkerAddress,
};
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::{
    ModelLifecycleError,
    state::{catalog_configuration, configuration},
};
use crate::{
    inference::{EngineBinding, EngineBindingIdentity},
    model::{
        domain::ModelDeploymentId,
        resolved::{ResolvedModelImageInputCapacity, ResolvedModelLaunch},
    },
};

const HTTP_ENDPOINT_NAME: &str = "http";
const BINDING_TOKEN_DOMAIN: &[u8] = b"dotcore.model-binding-token.v1\0";

/// A target kind that one serving-target view publishes, with a lease keyed by its deployment.
///
/// Each view carries exactly one kind, so a reader can only see the targets its writers publish:
/// the router reads node and upstream targets, and a node's private serving reads its local ones.
pub(crate) trait PublishedServingTarget: Clone + PartialEq + Send + Sync + 'static {
    fn deployment_id(&self) -> &ModelDeploymentId;
}

/// A cloneable private reader of one serving-target view.
///
/// The target type is a compile-time parameter so the router's view can never contain a local
/// runtime target; see [`PublishedServingTarget`].
#[derive(Clone)]
pub(crate) struct ServingTargetsSource<T: PublishedServingTarget = ServingTarget> {
    targets: Arc<RwLock<ServingTargets<T>>>,
    change_observers: Arc<ServingTargetChangeObservers>,
}

/// Reader of the node's own local runtime targets, for its status and private serving.
pub(crate) type LocalServingTargetsSource = ServingTargetsSource<LocalServingTarget>;

impl<T: PublishedServingTarget> ServingTargetsSource<T> {
    /// Registers a callback for changed controller-owned serving-target evidence.
    pub(crate) fn register_change_observer(&self, observer: Arc<dyn Fn() + Send + Sync>) {
        self.change_observers.register(observer);
    }

    /// Returns the complete atomic target view.
    pub(crate) fn snapshot(&self) -> ServingTargetsSnapshot<T> {
        let targets = self
            .targets
            .read()
            .unwrap_or_else(|value| value.into_inner());
        ServingTargetsSnapshot {
            targets: targets.targets.clone(),
        }
    }

    /// Selects one exact target and arms its deployment lease under the source lock.
    ///
    /// The selector must not perform dispatch or I/O; callers dispatch only after this method
    /// returns and the read lock has been released.
    pub(crate) fn admit<S, E>(
        &self,
        select: impl FnOnce(&[T]) -> Result<(T, S), E>,
    ) -> Result<Option<(S, ServingTargetsLease)>, E> {
        let targets = self
            .targets
            .read()
            .unwrap_or_else(|value| value.into_inner());
        let (target, selected) = select(&targets.targets)?;
        Ok(targets
            .targets
            .contains(&target)
            .then(|| targets.leases.lease_for(target.deployment_id()))
            .flatten()
            .map(|lease| (selected, lease)))
    }

    #[cfg(test)]
    pub(crate) fn lease_for(&self, target: &T) -> Option<ServingTargetsLease> {
        let targets = self
            .targets
            .read()
            .unwrap_or_else(|value| value.into_inner());
        targets
            .targets
            .contains(target)
            .then(|| targets.leases.lease_for(target.deployment_id()))
            .flatten()
    }
}

/// One complete atomic private serving-target snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServingTargetsSnapshot<T = ServingTarget> {
    pub(crate) targets: Vec<T>,
}

/// One target the inference router may select: a node-hosted model deployment, reached in
/// process or over the private channel, or an upstream provider.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServingTarget {
    Node(NodeServingTarget),

    Upstream(UpstreamServingTarget),
}

impl ServingTarget {
    pub(crate) fn deployment_id(&self) -> &ModelDeploymentId {
        match self {
            Self::Node(target) => &target.deployment_id,
            Self::Upstream(target) => &target.deployment_id,
        }
    }

    /// Returns the request kinds the target serves; upstream targets serve chat only.
    pub(crate) fn request_kinds(&self) -> BTreeSet<InferenceRequestKind> {
        match self {
            Self::Node(target) => target.request_kinds.clone(),
            Self::Upstream(_) => BTreeSet::from([InferenceRequestKind::Chat]),
        }
    }

    /// Returns source-neutral route availability evidence.
    pub(crate) fn route_eligibility(&self) -> ServingTargetEligibility {
        let (deployment_id, reconcile_generation, supports_text, supports_image) = match self {
            Self::Node(target) => (
                target.deployment_id.clone(),
                target.lifecycle_generation,
                true,
                target.image_capacity.is_some(),
            ),
            Self::Upstream(target) => (
                target.deployment_id.clone(),
                target.reconcile_generation,
                target.supports_text,
                target.supports_image,
            ),
        };
        ServingTargetEligibility {
            deployment_id,
            eligible: reconcile_generation > 0,
            supports_text,
            supports_image,
        }
    }
}

impl PublishedServingTarget for ServingTarget {
    fn deployment_id(&self) -> &ModelDeploymentId {
        ServingTarget::deployment_id(self)
    }
}

/// Source-neutral eligibility and capability evidence for a serving target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ServingTargetEligibility {
    pub(crate) deployment_id: ModelDeploymentId,
    pub(crate) eligible: bool,
    pub(crate) supports_text: bool,
    pub(crate) supports_image: bool,
}

/// One private local target and its adapter evidence.
#[derive(Clone)]
pub(crate) struct LocalServingTarget {
    pub(crate) model_id: CatalogModelId,
    pub(crate) engine_kind: i32,
    pub(crate) deployment_id: ModelDeploymentId,
    /// Declared lifecycle generation whose reconciliation published this target.
    pub(crate) lifecycle_generation: u64,
    pub(crate) reconcile_generation: u64,
    pub(crate) binding_identity: EngineBindingIdentity,
    pub(crate) binding: EngineBinding,
    pub(crate) image_capacity: Option<ResolvedModelImageInputCapacity>,

    /// Request kinds the deployment serves, from its immutable deployment snapshot.
    pub(crate) request_kinds: BTreeSet<InferenceRequestKind>,
}

impl LocalServingTarget {
    /// Returns the opaque binding token reported as worker serving evidence.
    ///
    /// The token is a domain-separated SHA-256 over the exact engine binding identity and the
    /// lifecycle generation it serves, rendered as lowercase hex. It names one binding without
    /// revealing an endpoint, so a replaced binding or a new generation always yields a new token.
    pub(crate) fn binding_token(&self) -> String {
        let identity = &self.binding_identity;
        let mut hasher = Sha256::new();
        hasher.update(BINDING_TOKEN_DOMAIN);
        for value in [
            identity.deployment_id(),
            identity.realization_id(),
            identity.service_name(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        hasher.update(identity.revision().to_be_bytes());
        hasher.update(self.lifecycle_generation.to_be_bytes());
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

impl fmt::Debug for LocalServingTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalServingTarget")
            .field("model_id", &self.model_id)
            .field("deployment_id", &self.deployment_id)
            .field("lifecycle_generation", &self.lifecycle_generation)
            .field("reconcile_generation", &self.reconcile_generation)
            .field("engine_kind", &self.engine_kind)
            .field("binding_identity", &self.binding_identity)
            .field("image_capacity", &self.image_capacity)
            .field("request_kinds", &self.request_kinds)
            .finish()
    }
}

impl PartialEq for LocalServingTarget {
    fn eq(&self, other: &Self) -> bool {
        self.model_id == other.model_id
            && self.deployment_id == other.deployment_id
            && self.lifecycle_generation == other.lifecycle_generation
            && self.reconcile_generation == other.reconcile_generation
            && self.engine_kind == other.engine_kind
            && self.binding_identity == other.binding_identity
            && self.image_capacity == other.image_capacity
            && self.request_kinds == other.request_kinds
    }
}

impl Eq for LocalServingTarget {}

impl PublishedServingTarget for LocalServingTarget {
    fn deployment_id(&self) -> &ModelDeploymentId {
        &self.deployment_id
    }
}

/// One node-hosted target that the manager reaches over the private peer channel.
///
/// It names one exact node realization: the hosting node, the manager session and node
/// incarnation that exchanged its evidence, the lifecycle generation it serves, and the opaque
/// binding token the node reported. Its dispatch view never implements Debug.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NodeServingTarget {
    pub(crate) deployment_id: ModelDeploymentId,
    pub(crate) lifecycle_generation: u64,
    pub(crate) site_node_id: String,

    /// Catalog model the deployment serves, from its immutable deployment snapshot.
    pub(crate) model_id: CatalogModelId,
    pub(crate) image_capacity: Option<ResolvedModelImageInputCapacity>,

    /// Request kinds the deployment serves, from its immutable deployment snapshot.
    pub(crate) request_kinds: BTreeSet<InferenceRequestKind>,
    dispatch: NodeDispatch,
}

impl NodeServingTarget {
    pub(crate) fn new(
        deployment_id: ModelDeploymentId,
        lifecycle_generation: u64,
        site_node_id: String,
        model_id: CatalogModelId,
        image_capacity: Option<ResolvedModelImageInputCapacity>,
        request_kinds: BTreeSet<InferenceRequestKind>,
        dispatch: NodeDispatch,
    ) -> Self {
        Self {
            deployment_id,
            lifecycle_generation,
            site_node_id,
            model_id,
            image_capacity,
            request_kinds,
            dispatch,
        }
    }

    /// Returns the dispatch view to the router adapter after target selection.
    pub(crate) fn dispatch(&self) -> &NodeDispatch {
        &self.dispatch
    }

    /// Replaces the request kinds the target serves.
    #[cfg(test)]
    pub(crate) fn with_request_kinds(
        mut self,
        request_kinds: BTreeSet<InferenceRequestKind>,
    ) -> Self {
        self.request_kinds = request_kinds;
        self
    }
}

impl fmt::Debug for NodeServingTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeServingTarget")
            .field("deployment_id", &self.deployment_id)
            .field("lifecycle_generation", &self.lifecycle_generation)
            .field("site_node_id", &self.site_node_id)
            .field("model_id", &self.model_id)
            .field("image_capacity", &self.image_capacity)
            .field("request_kinds", &self.request_kinds)
            .finish_non_exhaustive()
    }
}

/// Router-private data that reaches and names one exact node realization.
///
/// The transport says how the hosting node is reached; the session, incarnation, and binding
/// token let that node authorize exactly this target. It carries no credential.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct NodeDispatch {
    pub(crate) transport: NodeTransport,
    pub(crate) manager_session: ManagerSession,
    pub(crate) node_incarnation: NodeIncarnation,
    pub(crate) binding_token: String,
}

/// How the router reaches the node hosting one node target, chosen by node identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NodeTransport {
    /// Another node, over the private channel to its declared host and reported private port.
    Remote { address: WorkerAddress },

    /// The node sharing the router's process, through its local serving admission.
    InProcess,
}

/// One private eligible upstream target. Its dispatch view never implements Debug.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UpstreamServingTarget {
    pub(crate) deployment_id: ModelDeploymentId,
    pub(crate) reconcile_generation: u64,
    pub(crate) upstream_model_template_id: String,
    pub(crate) wire_protocol: UpstreamWireProtocol,
    pub(crate) supports_text: bool,
    pub(crate) supports_image: bool,
    pub(crate) boundary: UpstreamModelBoundary,
    dispatch: UpstreamDispatch,
}

impl fmt::Debug for UpstreamServingTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UpstreamServingTarget")
            .field("deployment_id", &self.deployment_id)
            .field("reconcile_generation", &self.reconcile_generation)
            .field(
                "upstream_model_template_id",
                &self.upstream_model_template_id,
            )
            .field("wire_protocol", &self.wire_protocol)
            .field("supports_text", &self.supports_text)
            .field("supports_image", &self.supports_image)
            .field("boundary", &self.boundary)
            .finish_non_exhaustive()
    }
}

/// Router-private dispatch configuration for an eligible upstream target.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct UpstreamDispatch {
    pub(crate) base_url: url::Url,
    pub(crate) upstream_model_id: String,
    pub(crate) api_key: String,
}

impl fmt::Debug for UpstreamDispatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("UpstreamDispatch([redacted])")
    }
}

impl UpstreamServingTarget {
    /// Returns the dispatch view to the router adapter after target selection.
    pub(crate) fn dispatch(&self) -> &UpstreamDispatch {
        &self.dispatch
    }
}

/// The private reason that invalidates all leases from one serving-target epoch.
pub(crate) type ServingTargetsRevocationReason = crate::inference::InferenceCancellationReason;

/// A private subscription tied to one serving-target publication epoch.
pub(crate) struct ServingTargetsLease {
    epoch: u64,
    revocation: watch::Receiver<Option<ServingTargetsRevocationReason>>,
}

impl ServingTargetsLease {
    pub(crate) fn into_parts(
        self,
    ) -> (u64, watch::Receiver<Option<ServingTargetsRevocationReason>>) {
        (self.epoch, self.revocation)
    }
}

/// The single writer of one serving-target view; clones share that same view.
#[derive(Clone)]
pub(crate) struct ServingTargetsPublisher<T: PublishedServingTarget = ServingTarget> {
    targets: Arc<RwLock<ServingTargets<T>>>,
    change_observers: Arc<ServingTargetChangeObservers>,
}

/// Writer of the node's own local runtime targets, owned by its model realizer.
pub(crate) type LocalServingTargetsPublisher = ServingTargetsPublisher<LocalServingTarget>;

impl<T: PublishedServingTarget> ServingTargetsPublisher<T> {
    pub(crate) fn new() -> (Self, ServingTargetsSource<T>) {
        let publisher = Self {
            targets: Arc::new(RwLock::new(ServingTargets::default())),
            change_observers: Arc::new(ServingTargetChangeObservers::default()),
        };
        let source = publisher.source();
        (publisher, source)
    }

    /// Returns a reader of the target view this publisher writes.
    pub(crate) fn source(&self) -> ServingTargetsSource<T> {
        ServingTargetsSource {
            targets: Arc::clone(&self.targets),
            change_observers: Arc::clone(&self.change_observers),
        }
    }

    /// Replaces the complete target view in one write operation.
    #[cfg(test)]
    pub(crate) fn publish(&self, targets: Vec<T>) {
        self.replace(|_| targets);
    }

    /// Removes every target when reconciliation cannot confirm their validity.
    pub(super) fn withdraw_all(&self) {
        self.replace(|_| Vec::new());
    }

    /// Removes targets that are not still valid before any runtime mutation.
    pub(super) fn retain_deployment(&self, deployment_id: &ModelDeploymentId, targets: &[T]) {
        self.replace(|mut published| {
            published.retain(|target| {
                target.deployment_id() != deployment_id || targets.contains(target)
            });
            published
        });
    }

    /// Replaces one deployment's targets while preserving every unrelated target.
    pub(super) fn upsert_deployment(&self, deployment_id: &ModelDeploymentId, targets: Vec<T>) {
        self.replace(|mut published| {
            published.retain(|target| target.deployment_id() != deployment_id);
            published.extend(targets);
            published
        });
    }

    /// Withdraws one deployment from its origin path before explicit unload returns.
    pub(super) fn withdraw_deployment(&self, deployment_id: &ModelDeploymentId) {
        self.replace(|mut published| {
            published.retain(|target| target.deployment_id() != deployment_id);
            published
        });
    }

    /// Replaces the view with `next(current)` under the write lock, revoking the lease of every
    /// deployment whose exact target changed or disappeared, then notifies observers on change.
    fn replace(&self, next: impl FnOnce(Vec<T>) -> Vec<T>) {
        let mut published = self
            .targets
            .write()
            .unwrap_or_else(|value| value.into_inner());
        let targets = next(published.targets.clone());
        let changed = published.replace(targets);
        drop(published);
        if changed {
            self.change_observers.notify();
        }
    }
}

impl ServingTargetsPublisher<ServingTarget> {
    /// Replaces every node target while preserving upstream targets.
    ///
    /// An unchanged node target keeps its lease, so only deployments whose exact target changed
    /// or disappeared are revoked.
    pub(crate) fn replace_node_targets(&self, targets: Vec<NodeServingTarget>) {
        self.replace(|mut published| {
            published.retain(|target| !matches!(target, ServingTarget::Node(_)));
            published.extend(targets.into_iter().map(ServingTarget::Node));
            published
        });
    }
}

#[derive(Default)]
struct ServingTargetChangeObservers {
    observers: RwLock<Vec<Arc<dyn Fn() + Send + Sync>>>,
}

impl ServingTargetChangeObservers {
    fn register(&self, observer: Arc<dyn Fn() + Send + Sync>) {
        self.observers
            .write()
            .unwrap_or_else(|value| value.into_inner())
            .push(observer);
    }

    fn notify(&self) {
        let observers = self
            .observers
            .read()
            .unwrap_or_else(|value| value.into_inner())
            .clone();
        for observer in observers {
            observer();
        }
    }
}

/// One published target view and the per-deployment leases of its targets.
struct ServingTargets<T> {
    targets: Vec<T>,
    leases: TargetLeaseRegistry,
}

impl<T> Default for ServingTargets<T> {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            leases: TargetLeaseRegistry::default(),
        }
    }
}

impl<T: PublishedServingTarget> ServingTargets<T> {
    /// Installs `targets`, revoking the leases of changed or removed deployments; returns whether
    /// the view changed.
    fn replace(&mut self, targets: Vec<T>) -> bool {
        self.leases.replace(&self.targets, &targets);
        if self.targets == targets {
            return false;
        }
        self.targets = targets;
        true
    }
}

#[derive(Default)]
struct TargetLeaseRegistry {
    next_epoch: u64,
    leases: HashMap<String, TargetLeasePublisher>,
}

struct TargetLeasePublisher {
    epoch: u64,
    revocation: watch::Sender<Option<ServingTargetsRevocationReason>>,
}

impl TargetLeaseRegistry {
    fn replace<T: PublishedServingTarget>(&mut self, previous: &[T], replacement: &[T]) {
        for target in previous {
            let id = target.deployment_id();
            if !replacement
                .iter()
                .any(|candidate| candidate.deployment_id() == id && candidate == target)
                && let Some(lease) = self.leases.remove(&id.0)
            {
                lease
                    .revocation
                    .send_replace(Some(ServingTargetsRevocationReason::RouteRevoked));
            }
        }
        for target in replacement {
            let id = target.deployment_id();
            if !self.leases.contains_key(&id.0) {
                let (revocation, _) = watch::channel(None);
                let epoch = self.next_epoch;
                self.next_epoch = self
                    .next_epoch
                    .checked_add(1)
                    .expect("serving-target revocation epoch exhausted");
                self.leases
                    .insert(id.0.clone(), TargetLeasePublisher { epoch, revocation });
            }
        }
    }

    fn lease_for(&self, deployment_id: &ModelDeploymentId) -> Option<ServingTargetsLease> {
        self.leases
            .get(&deployment_id.0)
            .map(|lease| ServingTargetsLease {
                epoch: lease.epoch,
                revocation: lease.revocation.subscribe(),
            })
    }
}

#[cfg(test)]
pub(crate) fn upstream_target_for_test(deployment_id: &str) -> UpstreamServingTarget {
    upstream_target_for_test_with_dispatch(
        deployment_id,
        "https://test.example/v1",
        "private-model",
        "test-secret",
    )
}

#[cfg(test)]
pub(crate) fn upstream_target_for_test_with_dispatch(
    deployment_id: &str,
    base_url: &str,
    upstream_model_id: &str,
    api_key: &str,
) -> UpstreamServingTarget {
    UpstreamServingTarget {
        deployment_id: ModelDeploymentId(deployment_id.to_owned()),
        reconcile_generation: 1,
        upstream_model_template_id: "test-upstream".to_owned(),
        wire_protocol: UpstreamWireProtocol::OpenaiCompatible,
        supports_text: true,
        supports_image: false,
        boundary: UpstreamModelBoundary {
            locality: 1,
            trust_domain: "test.example".to_owned(),
            data_egress_class: 1,
        },
        dispatch: UpstreamDispatch {
            base_url: base_url.parse().expect("test upstream URL"),
            upstream_model_id: upstream_model_id.to_owned(),
            api_key: api_key.to_owned(),
        },
    }
}

#[cfg(test)]
pub(crate) fn node_target_for_test(deployment_id: &str, binding_token: &str) -> NodeServingTarget {
    NodeServingTarget::new(
        ModelDeploymentId(deployment_id.to_owned()),
        1,
        "worker-one".to_owned(),
        CatalogModelId("fixture-model".to_owned()),
        None,
        BTreeSet::from([InferenceRequestKind::Chat]),
        NodeDispatch {
            transport: NodeTransport::Remote {
                address: "127.0.0.2:47123".parse().expect("worker address"),
            },
            manager_session: ManagerSession::parse("a".repeat(32)).expect("manager session"),
            node_incarnation: NodeIncarnation::parse("1".repeat(32)).expect("worker incarnation"),
            binding_token: binding_token.to_owned(),
        },
    )
}

#[cfg(test)]
pub(crate) fn local_target_for_test(
    deployment_id: &str,
    image_capacity: Option<ResolvedModelImageInputCapacity>,
) -> LocalServingTarget {
    let realization_id = format!("{deployment_id}-realization");
    let binding = crate::model::controller::test_support::test_engine_binding_for_values(
        deployment_id,
        &realization_id,
        "fixture-model",
        1,
        None,
    );
    LocalServingTarget {
        model_id: CatalogModelId("fixture-model".to_owned()),
        deployment_id: ModelDeploymentId(deployment_id.to_owned()),
        lifecycle_generation: 1,
        reconcile_generation: 1,
        engine_kind: EngineKind::LlamaCpp as i32,
        binding_identity: binding.identity().clone(),
        binding,
        image_capacity,
        request_kinds: BTreeSet::from([InferenceRequestKind::Chat]),
    }
}

/// Returns the request kinds a model deployment declares: its snapshot's kinds for a local
/// deployment, and chat only for an upstream one. Unknown wire values are ignored.
pub(crate) fn declared_request_kinds(
    declaration: &ModelDeploymentDeclaration,
) -> BTreeSet<InferenceRequestKind> {
    match declaration
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    {
        Some(Source::Local(local)) => local
            .request_kinds
            .iter()
            .filter_map(|value| InferenceRequestKind::try_from(*value).ok())
            .filter(|kind| *kind != InferenceRequestKind::Unspecified)
            .collect(),
        Some(Source::Upstream(_)) => BTreeSet::from([InferenceRequestKind::Chat]),
        None => BTreeSet::new(),
    }
}

/// Builds all READY routes before one atomic publication at reconciliation end.
pub(super) fn serving_targets<'a>(
    ready: impl IntoIterator<Item = ReadyServingTarget<'a>>,
) -> Result<Vec<LocalServingTarget>, ModelLifecycleError> {
    let mut targets = Vec::new();
    for target in ready {
        let resolved = ResolvedModelLaunch::from_declaration(target.deployment)
            .map_err(catalog_configuration)?;
        let model_id = resolved.model_id.clone();
        let deployment_id = ModelDeploymentId(target.deployment.model_deployment_id.clone());
        let service = target
            .workload
            .services
            .iter()
            .find(|service| service.reference.service_name == HTTP_ENDPOINT_NAME)
            .ok_or_else(|| configuration("READY runtime evidence has no http service binding"))?;
        let binding_identity = target.binding.identity().clone();
        if binding_identity.deployment_id() != deployment_id.0
            || binding_identity.realization_id() != service.reference.realization_id
            || binding_identity.service_name() != service.reference.service_name
            || binding_identity.revision() != service.reference.revision
        {
            return Err(configuration(
                "engine binding identity does not match READY runtime evidence",
            ));
        }
        targets.push(LocalServingTarget {
            model_id,
            deployment_id,
            lifecycle_generation: target.deployment.lifecycle_generation,
            reconcile_generation: target.workload.correlation.reconcile_generation,
            engine_kind: resolved.engine.kind,
            binding_identity,
            binding: target.binding,
            image_capacity: resolved.input_capabilities.image.clone(),
            request_kinds: declared_request_kinds(target.deployment),
        });
    }
    Ok(targets)
}

/// Builds one eligible generic remote route from generation-scoped declarations.
pub(super) fn upstream_serving_targets(
    deployment: &ModelDeploymentDeclaration,
) -> Result<Vec<ServingTarget>, ModelLifecycleError> {
    let upstream = deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
        .and_then(|source| match source {
            Source::Upstream(upstream) => Some(upstream),
            Source::Local(_) => None,
        })
        .ok_or_else(|| {
            configuration("desired deployment has no immutable upstream model snapshot")
        })?;
    let template = upstream
        .upstream_model_template
        .as_ref()
        .ok_or_else(|| configuration("Upstream model snapshot has no upstream model template"))?;
    let desired = deployment
        .upstream_desired_configuration
        .as_ref()
        .ok_or_else(|| configuration("Upstream model has no desired configuration"))?;
    let endpoint = desired
        .endpoint
        .as_ref()
        .ok_or_else(|| configuration("Upstream model has no endpoint configuration"))?;
    let boundary = desired
        .boundary
        .clone()
        .ok_or_else(|| configuration("Upstream model has no data-boundary metadata"))?;
    let input = desired
        .input_capabilities
        .as_ref()
        .ok_or_else(|| configuration("Upstream model has no declared input capabilities"))?;
    let wire_protocol = UpstreamWireProtocol::try_from(template.wire_protocol)
        .map_err(|_| configuration("Upstream wire protocol is invalid"))?;
    let base_url = url::Url::parse(&endpoint.base_url)
        .map_err(|_| configuration("Upstream model dispatch URL is invalid"))?;
    let target = UpstreamServingTarget {
        deployment_id: ModelDeploymentId(deployment.model_deployment_id.clone()),
        reconcile_generation: upstream_reconcile_generation(deployment),
        upstream_model_template_id: template.upstream_model_template_id.clone(),
        wire_protocol,
        supports_text: input.supports_text,
        supports_image: input.supports_image,
        boundary,
        dispatch: UpstreamDispatch {
            base_url,
            upstream_model_id: desired.upstream_model_id.clone(),
            api_key: desired.api_key.clone(),
        },
    };
    Ok(vec![ServingTarget::Upstream(target)])
}

fn upstream_reconcile_generation(deployment: &ModelDeploymentDeclaration) -> u64 {
    let projection = UpstreamServingGenerationProjection {
        deployment_id: deployment.model_deployment_id.clone(),
        snapshot: deployment.snapshot.as_ref().and_then(|snapshot| {
            match snapshot.source.as_ref() {
                Some(Source::Upstream(upstream)) => Some(upstream.clone()),
                Some(Source::Local(_)) | None => None,
            }
        }),
        desired_configuration: deployment.upstream_desired_configuration.clone(),
    };
    let digest = Sha256::digest(projection.encode_to_vec());
    u64::from_be_bytes(
        digest[..8]
            .try_into()
            .expect("SHA-256 prefix has eight bytes"),
    )
}

/// Canonical serving-relevant projection for one upstream model deployment.
///
/// Presentation metadata is deliberately absent so name-only mutations preserve
/// the private serving target generation.
#[derive(Clone, Message)]
struct UpstreamServingGenerationProjection {
    #[prost(string, tag = "1")]
    deployment_id: String,

    #[prost(message, optional, tag = "2")]
    snapshot: Option<UpstreamModelDeploymentSnapshot>,

    #[prost(message, optional, tag = "3")]
    desired_configuration: Option<UpstreamModelDesiredConfiguration>,
}

/// Validated READY evidence used only until the complete snapshot is published.
pub(super) struct ReadyServingTarget<'a> {
    pub(super) deployment: &'a ModelDeploymentDeclaration,
    pub(super) target: &'a ExecutionTarget,
    pub(super) workload: ObservedWorkload,
    pub(super) binding: EngineBinding,
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use dotcore_proto::dotcore::{
        catalog::v1::{
            ModelParallelShape, UpstreamModelBoundary, UpstreamModelDataEgressClass,
            UpstreamModelLocality, UpstreamModelTemplate, UpstreamWireProtocol,
        },
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentSnapshot,
            OpenAiCompatibleEndpointConfiguration, UpstreamModelDeploymentSnapshot,
            UpstreamModelDesiredConfiguration, UpstreamModelInputCapabilities,
            model_deployment_snapshot::Source as ModelDeploymentSnapshotSource,
        },
        site::v1::{
            DockerRuntimeAdapter, ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter,
            SimulatedRuntimeScenario, execution_target, model_runtime_binding,
        },
    };
    use dotcore_runtime::{RuntimeBackendId, WorkloadCorrelation, WorkloadId};

    use super::*;

    #[test]
    fn declared_request_kinds_follow_the_snapshot_and_upstreams_serve_chat_only() {
        let local = |kinds: &[InferenceRequestKind]| {
            let mut snapshot = dotcore_domain::test_support::model::local_model_snapshot("model");
            let Some(ModelDeploymentSnapshotSource::Local(local)) = snapshot.source.as_mut() else {
                panic!("local fixture snapshot");
            };
            local.request_kinds = kinds.iter().map(|kind| *kind as i32).collect();
            ModelDeploymentDeclaration {
                snapshot: Some(snapshot),
                ..Default::default()
            }
        };

        assert_eq!(
            declared_request_kinds(&local(&[
                InferenceRequestKind::Decision,
                InferenceRequestKind::Chat
            ])),
            BTreeSet::from([InferenceRequestKind::Chat, InferenceRequestKind::Decision])
        );
        assert_eq!(
            declared_request_kinds(&local(&[InferenceRequestKind::Decision])),
            BTreeSet::from([InferenceRequestKind::Decision])
        );
        assert_eq!(
            declared_request_kinds(&upstream_deployment("test-secret", "upstream-model")),
            BTreeSet::from([InferenceRequestKind::Chat])
        );
        assert_eq!(
            declared_request_kinds(&ModelDeploymentDeclaration::default()),
            BTreeSet::new()
        );
    }

    #[test]
    fn serving_targets_report_the_request_kinds_they_serve() {
        let decision = BTreeSet::from([InferenceRequestKind::Decision]);

        assert_eq!(
            ServingTarget::Node(node_target_for_test("node-model", "token")).request_kinds(),
            BTreeSet::from([InferenceRequestKind::Chat])
        );
        assert_eq!(
            ServingTarget::Node(
                node_target_for_test("node-model", "token").with_request_kinds(decision.clone())
            )
            .request_kinds(),
            decision
        );
        assert_eq!(
            ServingTarget::Upstream(upstream_target_for_test("upstream")).request_kinds(),
            BTreeSet::from([InferenceRequestKind::Chat])
        );
    }

    #[test]
    fn serving_target_change_observers_only_receive_changed_evidence() {
        let (publisher, source) = ServingTargetsPublisher::new();
        let observed = Arc::new(AtomicUsize::new(0));
        let observer_count = Arc::clone(&observed);
        source.register_change_observer(Arc::new(move || {
            observer_count.fetch_add(1, Ordering::SeqCst);
        }));

        let targets =
            upstream_serving_targets(&upstream_deployment("test-secret", "upstream-model"))
                .expect("upstream target");
        publisher.publish(targets);
        publisher.publish(Vec::new());
        publisher.publish(Vec::new());

        assert_eq!(observed.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn upstream_generation_excludes_presentation_name() {
        let deployment = upstream_deployment("test-secret", "upstream-model");
        let generation = upstream_reconcile_generation(&deployment);
        let mut renamed = deployment;
        renamed.model_deployment_name = "Renamed upstream deployment".to_owned();

        assert_eq!(upstream_reconcile_generation(&renamed), generation);
    }

    #[test]
    fn renaming_upstream_deployment_preserves_its_target_generation_and_lease() {
        let deployment = upstream_deployment("test-secret", "upstream-model");
        let (publisher, sources) = ServingTargetsPublisher::new();
        publisher.publish(upstream_serving_targets(&deployment).expect("initial upstream target"));
        let mut initial = sources.snapshot();
        let target = initial.targets.remove(0);
        let lease = sources.lease_for(&target).expect("initial target lease");
        let (epoch, mut revocation) = lease.into_parts();

        let mut renamed = deployment.clone();
        renamed.model_deployment_name = "Renamed deployment".to_owned();
        assert_eq!(
            upstream_reconcile_generation(&renamed),
            upstream_reconcile_generation(&deployment)
        );
        publisher.upsert_deployment(
            &ModelDeploymentId(deployment.model_deployment_id.clone()),
            upstream_serving_targets(&renamed).expect("renamed upstream target"),
        );

        assert_eq!(sources.snapshot().targets, vec![target.clone()]);
        let retained = sources.lease_for(&target).expect("retained target lease");
        assert_eq!(retained.into_parts().0, epoch);
        assert_eq!(*revocation.borrow(), None);

        let mut edited = renamed;
        edited
            .upstream_desired_configuration
            .as_mut()
            .expect("upstream desired configuration")
            .endpoint
            .as_mut()
            .expect("upstream endpoint")
            .base_url = "https://replacement.example/v1".to_owned();
        assert_ne!(
            upstream_reconcile_generation(&edited),
            upstream_reconcile_generation(&deployment)
        );
        publisher.upsert_deployment(
            &ModelDeploymentId(deployment.model_deployment_id),
            upstream_serving_targets(&edited).expect("edited upstream target"),
        );
        assert_eq!(
            *revocation.borrow_and_update(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
    }

    #[test]
    fn upstream_target_carries_complete_metadata_and_redacts_private_dispatch() {
        let deployment = upstream_deployment("test-secret", "upstream-model");
        let targets = upstream_serving_targets(&deployment).expect("upstream target");
        let [ServingTarget::Upstream(target)] = targets.as_slice() else {
            panic!("expected upstream target");
        };

        assert_eq!(target.deployment_id.0, "upstream-deployment");
        assert_eq!(target.upstream_model_template_id, "openai-compatible");
        assert_eq!(target.wire_protocol, UpstreamWireProtocol::OpenaiCompatible);
        assert!(target.supports_text);
        assert!(target.supports_image);
        assert_eq!(target.boundary.trust_domain, "endpoint.example");
        assert_eq!(
            target.dispatch().base_url.as_str(),
            "https://endpoint.example/v1"
        );
        assert_eq!(target.dispatch().upstream_model_id, "upstream-model");
        assert_eq!(target.dispatch().api_key, "test-secret");
        assert!(!format!("{target:?}").contains("test-secret"));
        assert!(!format!("{:?}", target.dispatch()).contains("test-secret"));

        let replacement = upstream_deployment("replacement-secret", "replacement-model");
        let replacement = upstream_serving_targets(&replacement).expect("replacement target");
        let [ServingTarget::Upstream(replacement)] = replacement.as_slice() else {
            panic!("expected replacement upstream target");
        };
        assert_ne!(
            target.reconcile_generation,
            replacement.reconcile_generation
        );
    }

    #[test]
    fn publication_projects_docker_and_simulated_runtime_targets_as_local() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let mut live = deployment("live-deployment");
        let live_model = live
            .snapshot
            .as_mut()
            .and_then(|snapshot| match snapshot.source.as_mut()? {
                ModelDeploymentSnapshotSource::Local(model) => Some(model),
                ModelDeploymentSnapshotSource::Upstream(_) => None,
            })
            .expect("Local model");
        live_model.model_id = "live-model".to_owned();
        live_model
            .desired_launch_plan
            .as_mut()
            .and_then(|plan| plan.arguments.get_mut(4))
            .expect("Local model alias argument")
            .value = Some(
            dotcore_proto::dotcore::model::v1::portable_model_launch_argument_token::Value::Literal(
                "live-model".to_owned(),
            ),
        );
        let simulated = deployment("simulated-deployment");
        let live_target = docker_target("live-target");
        let simulated_target = simulated_target("simulated-target");
        let live_workload = workload("live-deployment", 3, vec![endpoint(8080)]);
        let simulated_workload = workload("simulated-deployment", 5, Vec::new());
        let live_binding =
            crate::model::controller::test_support::test_engine_binding(&live, &live_workload);
        let simulated_binding = crate::model::controller::test_support::test_engine_binding(
            &simulated,
            &simulated_workload,
        );
        publisher.publish(
            serving_targets([
                ReadyServingTarget {
                    deployment: &live,
                    target: &live_target,
                    workload: live_workload,
                    binding: live_binding,
                },
                ReadyServingTarget {
                    deployment: &simulated,
                    target: &simulated_target,
                    workload: simulated_workload,
                    binding: simulated_binding,
                },
            ])
            .expect("serving paths"),
        );

        assert_eq!(
            published_ids(&sources),
            ["live-deployment", "simulated-deployment"]
        );
        publisher.withdraw_all();

        assert!(sources.snapshot().targets.is_empty());
    }

    #[test]
    fn unified_source_arms_the_selected_deployment_lease_before_withdrawal() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let local = deployment("local-deployment");
        let simulated = deployment("simulated-deployment");
        publish_both(&publisher, &local, &simulated);

        let source = &sources;
        let (deployment_id, lease) = source
            .admit(|targets| {
                let target = targets
                    .iter()
                    .find(|target| target.deployment_id.0 == "simulated-deployment")
                    .expect("published simulated target")
                    .clone();
                let deployment_id = target.deployment_id.0.clone();
                Ok::<_, ()>((target, deployment_id))
            })
            .expect("selection succeeds")
            .expect("exact target remains published while the source lock is held");
        assert_eq!(deployment_id, "simulated-deployment");

        publisher.withdraw_deployment(&ModelDeploymentId(deployment_id));

        assert_eq!(
            *lease.into_parts().1.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(published_ids(source), ["local-deployment"]);
    }

    #[test]
    fn unified_source_keeps_an_unrelated_deployment_lease_epoch_unchanged() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let local = deployment("local-deployment");
        let simulated = deployment("simulated-deployment");
        publish_both(&publisher, &local, &simulated);

        let source = &sources;
        let (_, local_lease) = source
            .admit(|targets| {
                let target = targets
                    .iter()
                    .find(|target| target.deployment_id.0 == "local-deployment")
                    .expect("published Local target")
                    .clone();
                Ok::<_, ()>((target, ()))
            })
            .expect("selection succeeds")
            .expect("Local target is published");
        let (epoch, revocation) = local_lease.into_parts();

        publisher.withdraw_deployment(&ModelDeploymentId("simulated-deployment".to_owned()));

        let (_, retained_lease) = source
            .admit(|targets| {
                let target = targets
                    .iter()
                    .find(|target| target.deployment_id.0 == "local-deployment")
                    .expect("retained Local target")
                    .clone();
                Ok::<_, ()>((target, ()))
            })
            .expect("selection succeeds")
            .expect("Local target remains published");
        assert_eq!(retained_lease.into_parts().0, epoch);
        assert_eq!(*revocation.borrow(), None);
    }

    #[test]
    fn serving_projection_uses_binding_evidence_without_runtime_adapter_checks() {
        let deployment = deployment("simulated-deployment");
        let target = simulated_target("simulated-target");
        let workload = workload("simulated-deployment", 1, vec![endpoint(8080)]);
        let binding =
            crate::model::controller::test_support::test_engine_binding(&deployment, &workload);

        let targets = serving_targets([ReadyServingTarget {
            deployment: &deployment,
            target: &target,
            workload,
            binding,
        }])
        .expect("generic serving target");

        assert!(matches!(
            targets.as_slice(),
            [target] if target.deployment_id.0 == "simulated-deployment"
        ));
    }

    #[test]
    fn serving_projection_keeps_binding_revision_independent_of_reconcile_generation() {
        let deployment = deployment("live-deployment");
        let target = docker_target("live-target");
        let mut workload = workload("live-deployment", 3, vec![endpoint(50000)]);
        workload.services = vec![WorkloadServiceBinding::loopback(
            workload.workload_id.0.clone(),
            HTTP_ENDPOINT_NAME,
            50000,
            endpoint(50000),
        )];
        let binding =
            crate::model::controller::test_support::test_engine_binding(&deployment, &workload);

        let targets = serving_targets([ReadyServingTarget {
            deployment: &deployment,
            target: &target,
            workload,
            binding,
        }])
        .expect("exact runtime binding can differ from deployment generation");
        let [target] = targets.as_slice() else {
            panic!("expected one Local serving target");
        };

        assert_eq!(target.reconcile_generation, 3);
        assert_eq!(target.binding_identity.revision(), 50000);
    }

    #[test]
    fn publication_replaces_the_complete_serving_snapshot() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let deployment = deployment("live-deployment");
        let target = docker_target("live-target");
        let workload = workload("live-deployment", 3, vec![endpoint(8080)]);
        let binding =
            crate::model::controller::test_support::test_engine_binding(&deployment, &workload);
        publisher.publish(
            serving_targets([ReadyServingTarget {
                deployment: &deployment,
                target: &target,
                workload,
                binding,
            }])
            .expect("Local serving path"),
        );

        publisher.withdraw_all();

        assert!(sources.snapshot().targets.is_empty());
    }

    #[test]
    fn changed_publication_revokes_only_its_origin_and_unchanged_publication_keeps_its_epoch() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let live = deployment("live-deployment");
        let simulated = deployment("simulated-deployment");
        publish_both(&publisher, &live, &simulated);
        let live_target = sources.snapshot().targets[0].clone();
        let simulated_target = sources.snapshot().targets[1].clone();
        let live_lease = sources.lease_for(&live_target).expect("live lease");
        let simulated_lease = sources
            .lease_for(&simulated_target)
            .expect("Simulated lease");
        let (live_epoch, live_revocation) = live_lease.into_parts();
        let (simulated_epoch, simulated_revocation) = simulated_lease.into_parts();

        publish_both(&publisher, &live, &simulated);
        let unchanged_live = sources.lease_for(&live_target).expect("live lease");
        let unchanged_simulated = sources
            .lease_for(&simulated_target)
            .expect("Simulated lease");
        assert_eq!(unchanged_live.into_parts().0, live_epoch);
        assert_eq!(unchanged_simulated.into_parts().0, simulated_epoch);
        assert_eq!(*live_revocation.borrow(), None);
        assert_eq!(*simulated_revocation.borrow(), None);

        publisher.withdraw_deployment(&ModelDeploymentId("live-deployment".to_owned()));

        assert_eq!(
            *live_revocation.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(*simulated_revocation.borrow(), None);
        assert_eq!(published_ids(&sources), ["simulated-deployment"]);
    }

    #[test]
    fn upstream_withdrawal_revokes_only_its_lease_and_preserves_node_continuity() {
        let (publisher, sources) = ServingTargetsPublisher::new();
        let node = ServingTarget::Node(node_target_for_test("node-deployment", &"b".repeat(64)));
        let upstream = upstream_serving_targets(&upstream_deployment("test-key", "gpt-4o"))
            .expect("upstream target")
            .remove(0);
        publisher.publish(vec![node.clone(), upstream.clone()]);
        let node_lease = sources.lease_for(&node).expect("node lease");
        let upstream_lease = sources.lease_for(&upstream).expect("upstream lease");

        publisher.withdraw_deployment(upstream.deployment_id());

        assert_eq!(*node_lease.into_parts().1.borrow(), None);
        assert_eq!(
            *upstream_lease.into_parts().1.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(sources.snapshot().targets, vec![node]);
    }

    #[test]
    fn per_deployment_retain_preserves_unrelated_targets_and_revokes_only_changed_origin() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let live = deployment("live-deployment");
        let simulated = deployment("simulated-deployment");
        publish_both(&publisher, &live, &simulated);
        let live_target = sources.snapshot().targets[0].clone();
        let simulated_target = sources.snapshot().targets[1].clone();
        let live_lease = sources.lease_for(&live_target).expect("live lease");
        let simulated_lease = sources
            .lease_for(&simulated_target)
            .expect("Simulated lease");
        let (_, live_revocation) = live_lease.into_parts();
        let (_, simulated_revocation) = simulated_lease.into_parts();

        publisher.retain_deployment(&ModelDeploymentId("live-deployment".to_owned()), &[]);

        assert_eq!(published_ids(&sources), ["simulated-deployment"]);
        assert_eq!(
            *live_revocation.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(*simulated_revocation.borrow(), None);
    }

    #[test]
    fn per_deployment_upsert_preserves_unrelated_targets_and_revokes_changed_origin() {
        let (publisher, sources) = LocalServingTargetsPublisher::new();
        let live = deployment("live-deployment");
        let simulated = deployment("simulated-deployment");
        publish_both(&publisher, &live, &simulated);
        let live_target = sources.snapshot().targets[0].clone();
        let simulated_target = sources.snapshot().targets[1].clone();
        let live_lease = sources.lease_for(&live_target).expect("live lease");
        let simulated_lease = sources
            .lease_for(&simulated_target)
            .expect("Simulated lease");
        let (_, live_revocation) = live_lease.into_parts();
        let (_, simulated_revocation) = simulated_lease.into_parts();
        let live_target = docker_target("live-target");
        let replacement_workload = workload("live-deployment", 7, vec![endpoint(9090)]);
        let replacement_binding = crate::model::controller::test_support::test_engine_binding(
            &live,
            &replacement_workload,
        );
        let replacement = serving_targets([ReadyServingTarget {
            deployment: &live,
            target: &live_target,
            workload: replacement_workload,
            binding: replacement_binding,
        }])
        .expect("replacement target");

        publisher.upsert_deployment(
            &ModelDeploymentId("live-deployment".to_owned()),
            replacement,
        );

        let snapshot = sources.snapshot();
        let replaced = snapshot
            .targets
            .iter()
            .find(|target| target.deployment_id.0 == "live-deployment")
            .expect("replaced Local target");
        assert_eq!(replaced.reconcile_generation, 7);
        assert_eq!(sources.snapshot().targets.len(), 2);
        assert_eq!(
            *live_revocation.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(*simulated_revocation.borrow(), None);
    }

    #[test]
    fn node_target_replacement_never_revokes_an_admitted_upstream_lease() {
        let (publisher, sources) = ServingTargetsPublisher::new();
        let upstream = upstream_serving_targets(&upstream_deployment("test-key", "private-model"))
            .expect("upstream target");
        publisher.upsert_deployment(
            &ModelDeploymentId("upstream-deployment".to_owned()),
            upstream,
        );
        let (target, lease) = sources
            .admit(|targets| {
                let [target] = targets else {
                    panic!("expected only the upstream target");
                };
                Ok::<_, ()>((target.clone(), target.clone()))
            })
            .expect("selection succeeds")
            .expect("upstream target is admitted");
        let (epoch, revocation) = lease.into_parts();

        publisher.replace_node_targets(vec![node_target_for_test(
            "node-deployment",
            &"b".repeat(64),
        )]);
        publisher.replace_node_targets(Vec::new());

        assert!(matches!(target, ServingTarget::Upstream(_)));
        assert_eq!(sources.snapshot().targets, vec![target.clone()]);
        let retained = sources.lease_for(&target).expect("retained upstream lease");
        assert_eq!(retained.into_parts().0, epoch);
        assert_eq!(*revocation.borrow(), None);
    }

    #[test]
    fn node_replacement_revokes_only_changed_deployments() {
        let (publisher, sources) = ServingTargetsPublisher::new();
        let kept = node_target_for_test("kept-deployment", &"b".repeat(64));
        let replaced = node_target_for_test("replaced-deployment", &"b".repeat(64));
        publisher.replace_node_targets(vec![kept.clone(), replaced.clone()]);
        let (kept_epoch, kept_revocation) = sources
            .lease_for(&ServingTarget::Node(kept.clone()))
            .expect("kept lease")
            .into_parts();
        let (_, replaced_revocation) = sources
            .lease_for(&ServingTarget::Node(replaced))
            .expect("replaced lease")
            .into_parts();

        let new_token = node_target_for_test("replaced-deployment", &"c".repeat(64));
        publisher.replace_node_targets(vec![kept.clone(), new_token.clone()]);

        assert_eq!(
            sources.snapshot().targets,
            vec![
                ServingTarget::Node(kept.clone()),
                ServingTarget::Node(new_token),
            ]
        );
        assert_eq!(
            *replaced_revocation.borrow(),
            Some(ServingTargetsRevocationReason::RouteRevoked)
        );
        assert_eq!(*kept_revocation.borrow(), None);
        assert_eq!(
            sources
                .lease_for(&ServingTarget::Node(kept))
                .expect("kept lease")
                .into_parts()
                .0,
            kept_epoch
        );
    }

    #[test]
    fn node_target_debug_omits_private_dispatch() {
        let target = node_target_for_test("node-deployment", &"b".repeat(64));
        let rendered = format!("{target:?}");

        assert!(rendered.contains("node-deployment"));
        assert!(!rendered.contains("127.0.0.2"));
        assert!(!rendered.contains("47123"));
        assert!(!rendered.contains(&"b".repeat(64)));
        assert!(!rendered.contains(&"1".repeat(32)));
    }

    fn publish_both(
        publisher: &LocalServingTargetsPublisher,
        live: &ModelDeploymentDeclaration,
        simulated: &ModelDeploymentDeclaration,
    ) {
        let live_target = docker_target("live-target");
        let simulated_target = simulated_target("simulated-target");
        let live_workload = workload(&live.model_deployment_id, 3, vec![endpoint(8080)]);
        let simulated_workload = workload(&simulated.model_deployment_id, 5, Vec::new());
        let live_binding =
            crate::model::controller::test_support::test_engine_binding(live, &live_workload);
        let simulated_binding = crate::model::controller::test_support::test_engine_binding(
            simulated,
            &simulated_workload,
        );
        publisher.publish(
            serving_targets([
                ReadyServingTarget {
                    deployment: live,
                    target: &live_target,
                    workload: live_workload,
                    binding: live_binding,
                },
                ReadyServingTarget {
                    deployment: simulated,
                    target: &simulated_target,
                    workload: simulated_workload,
                    binding: simulated_binding,
                },
            ])
            .expect("serving paths"),
        );
    }

    fn published_ids(sources: &LocalServingTargetsSource) -> Vec<String> {
        sources
            .snapshot()
            .targets
            .into_iter()
            .map(|target| target.deployment_id.0)
            .collect()
    }

    fn endpoint(host_port: u16) -> LoopbackEndpoint {
        LoopbackEndpoint {
            port_name: HTTP_ENDPOINT_NAME.to_owned(),
            host_port,
        }
    }

    fn workload(
        deployment_id: &str,
        generation: u64,
        loopback_endpoints: Vec<LoopbackEndpoint>,
    ) -> ObservedWorkload {
        ObservedWorkload {
            workload_id: WorkloadId(format!("workload-{deployment_id}")),
            correlation: WorkloadCorrelation {
                owner_id: deployment_id.to_owned(),
                reconcile_generation: generation,
            },
            backend_id: RuntimeBackendId("runtime/test".to_owned()),
            services: if loopback_endpoints.is_empty() {
                vec![WorkloadServiceBinding::native(
                    format!("workload-{deployment_id}"),
                    "http",
                    generation,
                )]
            } else {
                loopback_endpoints
                    .iter()
                    .cloned()
                    .map(|endpoint| {
                        WorkloadServiceBinding::loopback(
                            format!("workload-{deployment_id}"),
                            endpoint.port_name.clone(),
                            generation,
                            endpoint,
                        )
                    })
                    .collect()
            },
            loopback_endpoints,
            host_process: None,
        }
    }

    fn upstream_deployment(api_key: &str, upstream_model_id: &str) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: "upstream-deployment".to_owned(),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(ModelDeploymentSnapshotSource::Upstream(
                    UpstreamModelDeploymentSnapshot {
                        upstream_model_template: Some(UpstreamModelTemplate {
                            upstream_model_template_id: "openai-compatible".to_owned(),
                            display_name: "OpenAI-compatible".to_owned(),
                            wire_protocol: UpstreamWireProtocol::OpenaiCompatible as i32,
                        }),
                    },
                )),
            }),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: Some(UpstreamModelDesiredConfiguration {
                upstream_model_id: upstream_model_id.to_owned(),
                api_key: api_key.to_owned(),
                input_capabilities: Some(UpstreamModelInputCapabilities {
                    supports_text: true,
                    supports_image: true,
                }),
                endpoint: Some(OpenAiCompatibleEndpointConfiguration {
                    base_url: "https://endpoint.example/v1".to_owned(),
                    allow_insecure_http: false,
                }),
                boundary: Some(UpstreamModelBoundary {
                    locality: UpstreamModelLocality::PublicExternal as i32,
                    trust_domain: "endpoint.example".to_owned(),
                    data_egress_class: UpstreamModelDataEgressClass::PublicInternet as i32,
                }),
            }),
            ..Default::default()
        }
    }

    fn deployment(id: &str) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            model_deployment_name: "Model".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "node-one".to_owned(),
                execution_target_id: "target-one".to_owned(),
            }),
            requested_parallel_shape: ModelParallelShape::One as i32,
            snapshot: Some(snapshot()),
            desired_state: ModelDeploymentDesiredState::Present as i32,
            upstream_desired_configuration: None,
            ..Default::default()
        }
    }

    fn snapshot() -> ModelDeploymentSnapshot {
        crate::test_support::local_model_snapshot("fixture-model")
    }

    fn docker_target(id: &str) -> ExecutionTarget {
        model_target(
            id,
            model_runtime_binding::Adapter::DockerRuntime(DockerRuntimeAdapter::default()),
        )
    }

    fn simulated_target(id: &str) -> ExecutionTarget {
        ExecutionTarget {
            accelerator_device_ids: vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()],
            ..model_target(
                id,
                model_runtime_binding::Adapter::SimulatedRuntime(SimulatedRuntimeAdapter {
                    default_scenario: Some(SimulatedRuntimeScenario::default()),
                    deployment_scenarios: Vec::new(),
                }),
            )
        }
    }

    fn model_target(id: &str, adapter: model_runtime_binding::Adapter) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: id.to_owned(),
            display_name: id.to_owned(),
            site_node_id: "node-one".to_owned(),
            platform: None,
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(adapter),
                },
            )),
        }
    }
}
