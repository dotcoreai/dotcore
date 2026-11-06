//! Pins the development engine of each node-hosted model deployment once, on the node that hosts
//! it, before the node's realizer can launch it: a local-development image tag to its image ID, or
//! a package of the node's development source to the store path it evaluates to. A deployment
//! whose target runtime launches from artifact references alone, such as a simulated one, pins a
//! synthetic identity instead, without evaluating anything.
//!
//! The node's realization input withholds a deployment whose engine launch identity is neither
//! retained by this node nor projected back by the manager, and one whose launch, pinned, does
//! not resolve. Each reconciliation pass first resolves the withheld engines against their
//! retained platform, evaluating each distinct source at most once, retains every pin before the
//! deployment reaches the realizer, and records an unresolvable engine or launch as that
//! deployment's typed failure, so the node keeps realizing its other deployments. A retained or
//! projected pin is never resolved again, across pause, resume, and node restart. Shutting the
//! node down or changing its desired view cancels a pass in flight; the next pass resolves again.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
};

use dotcore_artifact::NixPinError;
use dotcore_catalog::CatalogError;
use dotcore_domain::deployment::launch_pin::{
    LaunchPinError, LaunchPinResolver, PinSource, SyntheticLaunchPins, UnpinnedSelection,
    pin_launch_identities,
};
use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    deployment::v1::NodeResolvedLaunchIdentity,
    model::v1::{
        CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentDesiredState,
        ModelDeploymentFailure, ModelDeploymentStatus,
    },
    site::store::v1::NodeResolvedLaunchIdentities,
};
use dotcore_runtime::WorkloadAssetMaterialization;
use dotcore_site::{
    repository::{ModelSiteSnapshot, SiteRepositoryError},
    store::FileNodeState,
    view::fencing::{LifecycleOperationAttempt, LifecycleOperationToken},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument as _, info, info_span, warn};

use super::{
    InferenceControllerInner, ModelLifecycleError, ModelRealizationInput,
    reconciliation::PassWakeup,
    state::{current_error, desired_error, failed_state},
};
use crate::model::{
    domain::ModelDeploymentId,
    engine_pin::{ENGINE_SELECTION_KEY, unpinned_engine_selection, with_node_pins},
    resolved::ResolvedModelLaunch,
    state::{CurrentStateError, CurrentStateRepository},
};

/// Failure code of a deployment whose development engine did not resolve on its node.
const LAUNCH_IDENTITY_UNRESOLVED_CODE: &str = "engine_launch_identity_unresolved";

/// Failure code of a deployment whose projected engine pin differs from the one its node
/// retained; only a new lifecycle generation or explicit recovery clears it.
const LAUNCH_IDENTITY_CONFLICT_CODE: &str = "engine_launch_identity_conflict";

/// Failure code of a deployment whose launch, with any engine pin applied, does not resolve on
/// its node; only a new declaration clears it.
const LAUNCH_INVALID_CODE: &str = "model_launch_invalid";

/// Status message of a deployment whose engine could not be pinned.
const ENGINE_ARTIFACT_UNAVAILABLE: &str = "Engine artifact unavailable";

/// Status message of a deployment whose launch does not resolve.
const LAUNCH_INVALID: &str = "Launch invalid";

/// Node realization input that applies the engine launch identities this node resolved, and
/// withholds every non-teardown deployment whose engine has no agreed pin yet or whose launch
/// does not resolve.
#[derive(Clone)]
pub(crate) struct NodePinnedModelInput<D> {
    input: D,
    pinning: Arc<NodeLaunchPinning>,
}

impl<D: ModelRealizationInput> NodePinnedModelInput<D> {
    /// Wraps the installed input of SiteNode `node_id`, loading the pins it retained in `state`.
    pub(crate) fn new(
        input: D,
        state: FileNodeState,
        node_id: String,
        resolver: Arc<dyn LaunchPinResolver>,
    ) -> Result<Self, CurrentStateError> {
        let retained = state
            .node_model_resolved_launch_identities()?
            .into_iter()
            .map(|record| (record.deployment_id, record.resolved_launch_identities))
            .collect();
        Ok(Self {
            input,
            pinning: Arc::new(NodeLaunchPinning {
                state,
                node_id,
                retained: RwLock::new(retained),
                resolver,
            }),
        })
    }

    /// Splits declarations into those the realizer receives, with pins applied, and those
    /// withheld until their engine is pinned or because their pinned launch does not resolve.
    fn partition(
        &self,
        declarations: Vec<ModelDeploymentDeclaration>,
    ) -> (
        Vec<ModelDeploymentDeclaration>,
        Vec<ModelDeploymentDeclaration>,
    ) {
        let mut pinned = Vec::with_capacity(declarations.len());
        let mut withheld = Vec::new();
        for declaration in declarations {
            match self.pinning.applied(&declaration) {
                Some(applied) if launch_error(&applied).is_none() => pinned.push(applied),
                _ => withheld.push(declaration),
            }
        }
        (pinned, withheld)
    }
}

impl<D: ModelRealizationInput> ModelRealizationInput for NodePinnedModelInput<D> {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        let snapshot = self.input.declaration_snapshot()?;
        Ok(ModelSiteSnapshot {
            deployments: self.partition(snapshot.deployments).0,
            ..snapshot
        })
    }

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(self.partition(self.input.model_deployments()?).0)
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.input.register_change_listener()
    }

    fn withheld_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        Ok(self.partition(self.input.model_deployments()?).1)
    }

    fn launch_pinning(&self) -> Option<&NodeLaunchPinning> {
        Some(&self.pinning)
    }
}

/// Engine launch identities one node resolved, retained durably in its current state, and the
/// node-local capability that resolves new ones.
pub(crate) struct NodeLaunchPinning {
    state: FileNodeState,
    node_id: String,
    retained: RwLock<BTreeMap<String, Vec<NodeResolvedLaunchIdentity>>>,
    resolver: Arc<dyn LaunchPinResolver>,
}

impl NodeLaunchPinning {
    /// Returns the launch identities this node retained for one deployment.
    pub(crate) fn retained(&self, deployment_id: &str) -> Vec<NodeResolvedLaunchIdentity> {
        self.retained
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(deployment_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Durably retains the launch identities resolved for one deployment, then exposes it.
    pub(crate) fn retain(
        &self,
        deployment_id: &str,
        pins: Vec<NodeResolvedLaunchIdentity>,
    ) -> Result<(), CurrentStateError> {
        self.state
            .retain_node_model_resolved_launch_identities(NodeResolvedLaunchIdentities {
                deployment_id: deployment_id.to_owned(),
                resolved_launch_identities: pins.clone(),
            })?;
        self.retained
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(deployment_id.to_owned(), pins);
        Ok(())
    }

    /// Returns the resolver of development launch identities on this node.
    pub(crate) fn resolver(&self) -> &dyn LaunchPinResolver {
        self.resolver.as_ref()
    }

    /// Returns the declaration with its engine pin applied, the declaration unchanged when it
    /// needs no pin, or `None` while its engine has no single agreed pin. A torn-down
    /// declaration is never withheld: it keeps its agreed pin when there is one, and is otherwise
    /// returned unchanged, since teardown does not need its launch.
    fn applied(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Option<ModelDeploymentDeclaration> {
        if unpinned_engine_selection(declaration).is_none() {
            return Some(declaration.clone());
        }
        let pinned = self.agreed_pin_applied(declaration);
        if declaration.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
            return Some(pinned.unwrap_or_else(|| declaration.clone()));
        }
        pinned
    }

    /// Returns the declaration with the single engine pin this node and the manager agree on,
    /// or `None` when there is no pin, the two disagree, or the pin is of another kind.
    fn agreed_pin_applied(
        &self,
        declaration: &ModelDeploymentDeclaration,
    ) -> Option<ModelDeploymentDeclaration> {
        let engine_pin = |pins: &[NodeResolvedLaunchIdentity]| {
            pins.iter()
                .find(|pin| pin.selection_key == ENGINE_SELECTION_KEY)
                .cloned()
        };
        let projected = engine_pin(&declaration.node_resolved_launch_identities);
        let retained = engine_pin(&self.retained(&declaration.model_deployment_id));
        let pin = match (projected, retained) {
            (Some(projected), Some(retained)) if projected != retained => return None,
            (Some(pin), _) | (None, Some(pin)) => pin,
            (None, None) => return None,
        };
        // A pin of another kind than the engine's source pins is withheld, so the pin pass
        // reports it as a conflict.
        let (selection, _) = unpinned_engine_selection(declaration)?;
        selection
            .source
            .kind()
            .admits(&pin.launch_identity)
            .then(|| with_node_pins(declaration, vec![pin]))
    }
}

impl<D, S, R> InferenceControllerInner<D, S, R>
where
    D: ModelRealizationInput,
    S: CurrentStateRepository,
    R: super::ModelRealizationResults,
{
    /// Resolves the engine of every withheld deployment once and retains its pin before the
    /// deployment reaches reconciliation. An engine that cannot be pinned, or a launch that does
    /// not resolve, fails only its own deployment, and is retried only for a later lifecycle
    /// generation. The pass stops early,
    /// recording nothing for the deployment in flight, when the realizer shuts down or the
    /// node's desired view changes.
    pub(super) async fn pin_withheld_engines(&self) -> Result<(), ModelLifecycleError> {
        let Some(pinning) = self.desired.launch_pinning() else {
            return Ok(());
        };
        let pass = self.shutdown.child_token();
        let view = self.declared_view()?;
        tokio::select! {
            result = self.pin_each_withheld(pinning, &pass) => result,
            never = self.cancel_on_view_change(&pass, view) => match never {},
        }
    }

    async fn pin_each_withheld(
        &self,
        pinning: &NodeLaunchPinning,
        pass: &CancellationToken,
    ) -> Result<(), ModelLifecycleError> {
        let resolver = PassResolver::new(pinning.resolver());
        for declaration in self.desired.withheld_deployments().map_err(desired_error)? {
            if pass.is_cancelled() {
                break;
            }
            let deployment_id = declaration.model_deployment_id.as_str();
            let record = self
                .state
                .snapshot()
                .map_err(current_error)?
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment_id);
            if record.as_ref().is_some_and(|record| {
                failed_for_generation(record, declaration.lifecycle_generation)
            }) {
                continue;
            }
            let operation = self.capture_operation(
                &declaration,
                LifecycleOperationAttempt::Runtime {
                    reconcile_generation: declaration.lifecycle_generation,
                },
            )?;

            // An applied pin means the deployment was withheld for its launch, not its engine.
            if let Some(error) = pinning
                .applied(&declaration)
                .and_then(|applied| launch_error(&applied))
            {
                let failure = launch_failure(&error, deployment_id, &pinning.node_id);
                warn!(
                    event_kind = "model_launch_invalid",
                    deployment_id,
                    lifecycle_generation = declaration.lifecycle_generation,
                    error_kind = %failure.code,
                    error = %dotcore_domain::diagnostic::diagnostic_chain(&error),
                    "deployment launch does not resolve on this node; the deployment fails"
                );
                self.record_failed(&operation, &declaration, record, LAUNCH_INVALID, failure)?;
                continue;
            }
            let Some((selection, platform)) = unpinned_engine_selection(&declaration) else {
                continue;
            };
            let span = info_span!(
                "model_engine_launch_identity",
                deployment_id,
                lifecycle_generation = declaration.lifecycle_generation,
            );
            // Kept apart from the pass's shared cache, which a real target of the same platform
            // may also fill for the same source.
            let resolver: &dyn LaunchPinResolver = if self.launches_from_references(&declaration) {
                &SyntheticLaunchPins
            } else {
                &resolver
            };
            let pinned = pin_launch_identities(
                std::slice::from_ref(&selection),
                &declaration.node_resolved_launch_identities,
                &pinning.retained(deployment_id),
                &platform,
                Some(resolver),
                pass,
            )
            .instrument(span)
            .await;
            match pinned {
                Ok(pins) => {
                    let pin = pins
                        .iter()
                        .find(|pin| pin.selection_key == ENGINE_SELECTION_KEY)
                        .map(|pin| pin.launch_identity.clone())
                        .unwrap_or_default();
                    self.commit_operation(&operation, || pinning.retain(deployment_id, pins))?
                        .map_err(current_error)?;
                    log_pinned(&declaration, &selection, &pin);
                }
                // A cancelled pass leaves the deployment withheld; the next pass resolves again.
                Err(_) if pass.is_cancelled() => break,
                Err(error) => {
                    let failure = pin_failure(&error, &selection, &pinning.node_id);
                    warn!(
                        event_kind = "model_engine_launch_identity_unpinned",
                        deployment_id,
                        lifecycle_generation = declaration.lifecycle_generation,
                        source = ?selection.source,
                        error_kind = %failure.code,
                        error = %dotcore_domain::diagnostic::diagnostic_chain(&error),
                        "development engine could not be pinned; the deployment fails"
                    );
                    self.record_failed(
                        &operation,
                        &declaration,
                        record,
                        ENGINE_ARTIFACT_UNAVAILABLE,
                        failure,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// Whether the deployment's target runtime launches from artifact references alone, such as
    /// the simulated one, so its engine pins a synthetic identity. A target this node doesn't
    /// run pins through the node's resolver, and reconciliation reports the target.
    fn launches_from_references(&self, declaration: &ModelDeploymentDeclaration) -> bool {
        declaration.placement.as_ref().is_some_and(|placement| {
            self.runtimes
                .materialization_requirements(&placement.execution_target_id)
                .is_ok_and(|requirements| {
                    requirements.executable_artifact == WorkloadAssetMaterialization::ReferenceOnly
                })
        })
    }

    /// Records a withheld deployment's typed failure for its declared lifecycle generation,
    /// replacing the `record` it had, and withdraws any serving target it still had: a withheld
    /// deployment never reaches the reconciliation that would withdraw it.
    fn record_failed(
        &self,
        operation: &LifecycleOperationToken,
        declaration: &ModelDeploymentDeclaration,
        record: Option<CurrentModelDeployment>,
        status_message: &str,
        failure: ModelDeploymentFailure,
    ) -> Result<(), ModelLifecycleError> {
        let failed = CurrentModelDeployment {
            model_deployment_id: declaration.model_deployment_id.clone(),
            reconciled_lifecycle_generation: Some(declaration.lifecycle_generation),
            record_revision: 0,
            recovery_barrier: None,
            state: Some(failed_state(
                record.as_ref().and_then(|record| record.state.as_ref()),
                status_message,
                failure,
            )),
        };
        let expected = record.as_ref().map(|record| record.record_revision);
        self.commit_operation(operation, || self.state.upsert(failed, expected))?
            .map_err(current_error)?;
        self.serving_targets_publisher
            .withdraw_deployment(&ModelDeploymentId(declaration.model_deployment_id.clone()));
        Ok(())
    }

    /// Cancels `pass` once the node's desired view differs from `view`, then waits forever. Every
    /// observer wakeup this wait consumes is raised again when the wait is dropped.
    async fn cancel_on_view_change(
        &self,
        pass: &CancellationToken,
        view: BTreeSet<DeclaredDeployment>,
    ) -> std::convert::Infallible {
        let changed = self.desired.register_change_listener();
        let mut wakeup = PassWakeup::new(&changed);
        loop {
            wakeup.notified().await;
            if self.declared_view().ok().as_ref() != Some(&view) {
                pass.cancel();
                break;
            }
        }
        std::future::pending().await
    }

    /// Identity, generation, and desired state of every deployment in the node's desired view,
    /// pinned or withheld; retaining a pin leaves it unchanged.
    fn declared_view(&self) -> Result<BTreeSet<DeclaredDeployment>, ModelLifecycleError> {
        let pinned = self.desired.model_deployments().map_err(desired_error)?;
        let withheld = self.desired.withheld_deployments().map_err(desired_error)?;
        Ok(pinned
            .iter()
            .chain(&withheld)
            .map(|declaration| {
                (
                    declaration.model_deployment_id.clone(),
                    declaration.lifecycle_generation,
                    declaration.desired_state,
                )
            })
            .collect())
    }
}

/// Deployment ID, lifecycle generation, and desired state of one declared deployment.
type DeclaredDeployment = (String, u64, i32);

/// Whether the record already reports a failure of exactly this lifecycle generation.
fn failed_for_generation(record: &CurrentModelDeployment, lifecycle_generation: u64) -> bool {
    record.reconciled_lifecycle_generation == Some(lifecycle_generation)
        && record.state.as_ref().is_some_and(|state| {
            ModelDeploymentStatus::try_from(state.status) == Ok(ModelDeploymentStatus::Failed)
        })
}

/// Returns why a declaration that may launch does not resolve to a launch, or `None` when it
/// resolves or is torn down, since teardown never needs its launch.
fn launch_error(declaration: &ModelDeploymentDeclaration) -> Option<CatalogError> {
    if declaration.desired_state == ModelDeploymentDesiredState::Teardown as i32 {
        return None;
    }
    ResolvedModelLaunch::from_declaration(declaration).err()
}

/// Returns the typed failure of a deployment whose launch does not resolve on its node.
fn launch_failure(
    error: &CatalogError,
    deployment_id: &str,
    node_id: &str,
) -> ModelDeploymentFailure {
    ModelDeploymentFailure {
        code: LAUNCH_INVALID_CODE.to_owned(),
        message: format!(
            "The launch of deployment {deployment_id:?} cannot be realized on node {node_id:?}: {error}; tear the deployment down and create it again"
        ),
        diagnostics: vec![dotcore_domain::diagnostic::diagnostic_chain(error)],
    }
}

/// Logs one engine pin retained before launch, with what it was resolved from.
fn log_pinned(declaration: &ModelDeploymentDeclaration, selection: &UnpinnedSelection, pin: &str) {
    let deployment_id = declaration.model_deployment_id.as_str();
    let lifecycle_generation = declaration.lifecycle_generation;
    match &selection.source {
        PinSource::LocalImage { reference } => info!(
            event_kind = "model_engine_launch_identity_pinned",
            deployment_id,
            lifecycle_generation,
            image = %reference,
            image_id = %pin,
            "local-development engine image pinned on this node before launch"
        ),
        PinSource::NixDevelopmentSource { package, .. } => info!(
            event_kind = "model_engine_launch_identity_pinned",
            deployment_id,
            lifecycle_generation,
            package = %package,
            system = dotcore_domain::nix::host_system().unwrap_or_default(),
            store_path = %pin,
            "development-source Nix engine pinned on this node before launch"
        ),
        PinSource::NixLocked { .. } => info!(
            event_kind = "model_engine_launch_identity_pinned",
            deployment_id,
            lifecycle_generation,
            system = dotcore_domain::nix::host_system().unwrap_or_default(),
            store_path = %pin,
            "locked Nix engine pinned to this node's Nix system before launch"
        ),
    }
}

/// Returns the typed failure of an engine that could not be pinned, worded for its source kind
/// and cause; a Nix evaluation failure carries the tail of what Nix reported.
fn pin_failure(
    error: &LaunchPinError,
    selection: &UnpinnedSelection,
    node_id: &str,
) -> ModelDeploymentFailure {
    let unresolved = |message: String, diagnostics: Vec<String>| ModelDeploymentFailure {
        code: LAUNCH_IDENTITY_UNRESOLVED_CODE.to_owned(),
        message,
        diagnostics,
    };
    let source = match error {
        LaunchPinError::Unresolved { source, .. } => source,
        LaunchPinError::ProjectedMismatch { .. } => {
            return ModelDeploymentFailure {
                code: LAUNCH_IDENTITY_CONFLICT_CODE.to_owned(),
                message: "The recorded engine artifact differs from the one this node pinned; pause and resume the deployment to recover".to_owned(),
                diagnostics: Vec::new(),
            };
        }
        LaunchPinError::KindMismatch { .. } => {
            return ModelDeploymentFailure {
                code: LAUNCH_IDENTITY_CONFLICT_CODE.to_owned(),
                message: "The recorded engine pin is not the kind of artifact this engine launches; pause and resume the deployment to recover".to_owned(),
                diagnostics: Vec::new(),
            };
        }
    };
    let package = match &selection.source {
        PinSource::LocalImage { .. } => {
            return unresolved(
                "The local-development engine image is not available on this node for the deployment's platform; build or tag it there, then pause and resume the deployment".to_owned(),
                Vec::new(),
            );
        }
        PinSource::NixDevelopmentSource { package, .. } => package,
        PinSource::NixLocked { .. } => {
            return match source
                .chain()
                .find_map(|cause| cause.downcast_ref::<NixPinError>())
            {
                Some(NixPinError::StorePathUnavailable { system }) => unresolved(
                    format!(
                        "Node {node_id:?} builds Nix artifacts for {system}, but the locked engine artifact has no store path for that system; deploy it to a node of a system the artifact supports"
                    ),
                    Vec::new(),
                ),
                _ => unresolved(
                    format!(
                        "The locked Nix engine artifact could not be pinned on node {node_id:?}: {}; pause and resume the deployment to retry",
                        dotcore_domain::diagnostic::diagnostic_chain(source.as_ref())
                    ),
                    Vec::new(),
                ),
            };
        }
    };
    match source
        .chain()
        .find_map(|cause| cause.downcast_ref::<NixPinError>())
    {
        Some(NixPinError::DevelopmentSourceUnavailable { .. }) => unresolved(
            format!(
                "Node {node_id:?} has no development source to evaluate engine package {package:?} from; set DOTCORE_DEV_SOURCE_FLAKE on that node, then pause and resume the deployment"
            ),
            Vec::new(),
        ),
        Some(NixPinError::Evaluation { source, .. }) => {
            evaluation_failure(source, package, node_id)
        }
        Some(other) => unresolved(
            format!(
                "Engine package {package:?} could not be pinned on node {node_id:?}: {other}; pause and resume the deployment to retry"
            ),
            Vec::new(),
        ),
        None => unresolved(
            format!(
                "Engine package {package:?} could not be pinned on node {node_id:?}: {}; pause and resume the deployment to retry",
                dotcore_domain::diagnostic::diagnostic_chain(source.as_ref())
            ),
            Vec::new(),
        ),
    }
}

/// Returns the failure of a development-source evaluation: what Nix reported when it ran and
/// failed, or why it could not run or finish.
fn evaluation_failure(
    error: &dotcore_artifact::NixEvaluationError,
    package: &str,
    node_id: &str,
) -> ModelDeploymentFailure {
    use dotcore_artifact::NixEvaluationError;

    let (message, diagnostics) = match error {
        NixEvaluationError::Failed { stderr, .. } => (
            format!(
                "Engine package {package:?} could not be evaluated from the development source of node {node_id:?}; fix the source on that node, then pause and resume the deployment"
            ),
            if stderr.is_empty() {
                Vec::new()
            } else {
                vec![format!("Nix reported: {stderr}")]
            },
        ),
        NixEvaluationError::NixMissing => (
            format!(
                "Node {node_id:?} cannot evaluate engine package {package:?}: {error}; install Nix on that node, then pause and resume the deployment"
            ),
            Vec::new(),
        ),
        NixEvaluationError::TimedOut { timeout, .. } => (
            format!(
                "Evaluating engine package {package:?} on node {node_id:?} did not finish within {timeout:?}; check that its development source and inputs are reachable from that node, then pause and resume the deployment"
            ),
            Vec::new(),
        ),
        other => (
            format!(
                "Engine package {package:?} could not be evaluated on node {node_id:?}; pause and resume the deployment to retry"
            ),
            vec![dotcore_domain::diagnostic::diagnostic_chain(other)],
        ),
    };
    ModelDeploymentFailure {
        code: LAUNCH_IDENTITY_UNRESOLVED_CODE.to_owned(),
        message,
        diagnostics,
    }
}

/// Resolver of one pinning pass that resolves each source for each platform at most once and
/// shares the outcome, so deployments of the same development engine share one evaluation.
struct PassResolver<'a> {
    resolver: &'a dyn LaunchPinResolver,
    outcomes: Mutex<BTreeMap<PassKey, Result<String, Arc<anyhow::Error>>>>,
}

/// Source and retained platform (operating system, architecture, variant) of one resolution.
type PassKey = (PinSource, (String, String, Option<String>));

impl<'a> PassResolver<'a> {
    fn new(resolver: &'a dyn LaunchPinResolver) -> Self {
        Self {
            resolver,
            outcomes: Mutex::new(BTreeMap::new()),
        }
    }

    fn outcome(&self, key: &PassKey) -> Option<Result<String, Arc<anyhow::Error>>> {
        self.outcomes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
    }
}

impl LaunchPinResolver for PassResolver<'_> {
    fn resolve_pin<'b>(
        &'b self,
        source: &'b PinSource,
        platform: &'b OciPlatform,
        cancellation: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<String, anyhow::Error>> + Send + 'b>> {
        Box::pin(async move {
            let key = (
                source.clone(),
                (
                    platform.operating_system.clone(),
                    platform.architecture.clone(),
                    platform.variant.clone(),
                ),
            );
            let outcome = match self.outcome(&key) {
                Some(outcome) => outcome,
                None => {
                    let outcome = self
                        .resolver
                        .resolve_pin(source, platform, cancellation)
                        .await
                        .map_err(Arc::new);
                    self.outcomes
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .insert(key, outcome.clone());
                    outcome
                }
            };
            outcome.map_err(|error| anyhow::Error::new(SharedPinError(error)))
        })
    }
}

/// A resolution failure shared by every deployment of a pass that needed the same source. Its
/// `Error` impl is manual because the shared `anyhow::Error` is not itself a `std` error source.
#[derive(Debug)]
struct SharedPinError(Arc<anyhow::Error>);

impl std::fmt::Display for SharedPinError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("engine artifact resolution failed")
    }
}

impl std::error::Error for SharedPinError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::{
        catalog::v1::OciPlatform,
        model::v1::{ModelDeploymentLifecycleTransition, ModelDeploymentState},
        site::v1::Site,
    };
    use dotcore_runtime::{
        RuntimeBackend, RuntimeBackendId, RuntimeFuture, WorkloadId, WorkloadLaunch,
        WorkloadMaterializationRequirements, WorkloadObservation, WorkloadStartupObserver,
        WorkloadStartupRequest,
    };
    use dotcore_runtime_backends::simulated::InMemoryRuntimeBackend;

    use super::*;
    use crate::{
        dispatch::model::{RuntimeTargetMap, RuntimeTargetMapBuilder},
        model::controller::{
            ArtifactDependencies, ModelControllerDependencies, ModelRealizer,
            NodeModelRealizerDependencies,
            serving_targets::ServingTargetsPublisher,
            test_support::{
                ProjectedTestInput, test_desired_deployment, test_engine_integrations, test_site,
                test_target_capability_source,
            },
        },
        test_support::TAG_ONLY_ENGINE_REFERENCE,
    };

    const PINNED: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const REBUILT: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const STORE_PATH: &str = dotcore_domain::test_support::nix::STORE_PATH;
    const REBUILT_STORE_PATH: &str = "/nix/store/00000000000000000000000000000002-llama-cpp-b11418";

    #[tokio::test]
    async fn a_worker_pins_its_engine_once_and_launches_only_that_image() {
        let node = Node::new();
        let images = TestPins::with(PINNED);
        let realizer = node.realizer(&images);

        let ready = reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;
        assert_eq!(ready.failure, None);
        assert_eq!(images.resolutions(), [TAG_ONLY_ENGINE_REFERENCE]);
        assert_eq!(node.created_images(), [PINNED]);
        assert_eq!(node.retained(), [engine_pin(PINNED)]);

        // A rebuilt tag is never picked up: not by pause and resume, nor by a node restart.
        images.retag(REBUILT);
        node.declare(paused(2));
        reconcile_until(&realizer, &node, ModelDeploymentStatus::Paused).await;
        node.declare(resumed(3));
        reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;
        drop(realizer);
        let restarted = node.realizer(&images);
        reconcile_until(&restarted, &node, ModelDeploymentStatus::Ready).await;

        // The manager projecting the same pin back changes nothing either.
        let mut projected = resumed(3);
        projected.node_resolved_launch_identities = vec![engine_pin(PINNED)];
        node.declare(projected);
        reconcile_until(&restarted, &node, ModelDeploymentStatus::Ready).await;

        assert_eq!(images.resolutions(), [TAG_ONLY_ENGINE_REFERENCE]);
        assert_eq!(node.created_images(), [PINNED]);
        assert_eq!(node.retained(), [engine_pin(PINNED)]);
    }

    #[tokio::test]
    async fn a_target_launching_from_references_pins_a_synthetic_engine_without_the_node_resolver()
    {
        let mut node = Node::new();
        node.runtime.requirements = WorkloadMaterializationRequirements::virtualized();
        let images = TestPins::with(PINNED);
        let realizer = node.realizer(&images);
        let (selection, platform) =
            unpinned_engine_selection(&present(1)).expect("an unpinned engine");
        let synthetic =
            SyntheticLaunchPins::identity(&selection.source, &platform).expect("a synthetic pin");

        let ready = reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;

        assert_eq!(ready.failure, None);
        assert!(
            images.resolutions().is_empty(),
            "the node resolver is not consulted"
        );
        assert_eq!(node.retained(), [engine_pin(&synthetic)]);
        assert_eq!(node.created_images(), [synthetic]);
    }

    #[tokio::test]
    async fn an_unresolvable_engine_fails_its_deployment_until_a_new_lifecycle_generation() {
        let node = Node::new();
        let images = TestPins::default();
        let realizer = node.realizer(&images);

        let failed = reconcile_until(&realizer, &node, ModelDeploymentStatus::Failed).await;
        realizer.inner.reconcile().await.expect("failed pass");
        assert_eq!(
            failed.failure.map(|failure| failure.code),
            Some(LAUNCH_IDENTITY_UNRESOLVED_CODE.to_owned())
        );
        assert_eq!(
            images.resolutions(),
            [TAG_ONLY_ENGINE_REFERENCE],
            "a failed generation is not retried"
        );
        assert!(node.created_images().is_empty());
        assert!(node.retained().is_empty());

        images.retag(PINNED);
        node.declare(paused(2));
        realizer.inner.reconcile().await.expect("pause pass");
        assert_eq!(node.retained(), [engine_pin(PINNED)]);
        node.declare(resumed(3));
        reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;
        assert_eq!(images.resolutions().len(), 2);
        assert_eq!(node.created_images(), [PINNED]);
    }

    #[tokio::test]
    async fn a_projected_pin_that_contradicts_the_retained_one_fails_without_launching() {
        let node = Node::new();
        node.state
            .retain_node_model_resolved_launch_identities(NodeResolvedLaunchIdentities {
                deployment_id: "deployment".to_owned(),
                resolved_launch_identities: vec![engine_pin(PINNED)],
            })
            .expect("retained pin");
        let mut contradicted = present(1);
        contradicted.node_resolved_launch_identities = vec![engine_pin(REBUILT)];
        node.declare(contradicted);
        let images = TestPins::with(PINNED);
        let realizer = node.realizer(&images);

        let failed = reconcile_until(&realizer, &node, ModelDeploymentStatus::Failed).await;

        assert_eq!(
            failed.failure.map(|failure| failure.code),
            Some(LAUNCH_IDENTITY_CONFLICT_CODE.to_owned())
        );
        assert!(images.resolutions().is_empty());
        assert!(node.created_images().is_empty());
    }

    #[tokio::test]
    async fn a_teardown_without_a_pin_reaches_the_realizer_and_others_wait_for_theirs() {
        let node = Node::new();
        let input = NodePinnedModelInput::new(
            node.input.clone(),
            node.state.clone(),
            "node-one".to_owned(),
            Arc::new(TestPins::default()),
        )
        .expect("pinned input");
        assert!(input.model_deployments().expect("deployments").is_empty());
        assert_eq!(
            input.withheld_deployments().expect("withheld"),
            [present(1)]
        );

        let mut teardown = present(2);
        teardown.desired_state = ModelDeploymentDesiredState::Teardown as i32;
        node.declare(teardown.clone());
        assert_eq!(input.model_deployments().expect("deployments"), [teardown]);
        assert!(input.withheld_deployments().expect("withheld").is_empty());
    }

    #[tokio::test]
    async fn node_logs_read_the_pinned_engine_launch_before_the_manager_projects_the_pin() {
        let node = Node::new();
        let images = TestPins::with(PINNED);
        let input = node.pinned_input(&images);
        let started = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let artifacts =
            dotcore_artifact::ArtifactManager::new(node._directory.path().join("cache"), None)
                .expect("artifact paths");
        let pinned_logs = crate::model::controller::NodeModelLogs::new(
            input.clone(),
            node.runtimes(),
            artifacts.clone(),
            Arc::clone(&started),
            crate::deployment_logs::PreparationLogs::on_system_clock(node._directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        let raw_logs = crate::model::controller::NodeModelLogs::new(
            node.input.clone(),
            node.runtimes(),
            artifacts,
            started,
            crate::deployment_logs::PreparationLogs::on_system_clock(node._directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        let query = || {
            crate::management_logs::DeploymentLogReadQuery::new(
                Vec::new(),
                dotcore_runtime::RuntimeLogQuery::new(
                    std::num::NonZeroU16::new(1).expect("limit"),
                    None,
                )
                .expect("query"),
            )
        };
        use crate::deployment_logs::DeploymentLogOwner as _;

        let awaiting_pin = pinned_logs
            .read_logs("deployment", query())
            .await
            .expect("an unpinned deployment reads as not created")
            .sources;
        let realizer = node.realizer_for(input);
        reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;
        let pinned = pinned_logs
            .read_logs("deployment", query())
            .await
            .expect("history of the pinned launch")
            .sources;
        let raw = raw_logs.read_logs("deployment", query()).await;

        assert!(matches!(
            awaiting_pin[0].result,
            Err(dotcore_runtime::RuntimeBackendError::WorkloadNotCreated)
        ));
        let entries = &pinned[0]
            .result
            .as_ref()
            .expect("the pinned workload is read")
            .entries;
        assert_eq!(entries.len(), 1);
        assert!(
            String::from_utf8_lossy(&entries[0].content)
                .starts_with("simulated workload log line 012 owner=deployment "),
        );
        assert!(
            matches!(
                raw,
                Err(crate::management_logs::DeploymentLogError::Configuration(_))
            ),
            "the unprojected view alone cannot resolve the launch"
        );
    }

    #[tokio::test]
    async fn a_node_pins_its_development_source_engine_once_and_launches_from_that_store_path() {
        let node = Node::new();
        node.declare(present_nix("deployment", 1));
        let pins = TestPins::evaluating(STORE_PATH);
        let input = node.pinned_input(&pins);
        let realizer = node.realizer_for(input.clone());
        assert!(input.model_deployments().expect("deployments").is_empty());

        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");

        assert_eq!(pins.resolutions(), ["engines.llama-cpp.native"]);
        assert_eq!(node.retained(), [engine_pin(STORE_PATH)]);
        assert_eq!(launch_identity(&input), STORE_PATH);

        // A changed source is never picked up: not by pause and resume, a realizer restart, or
        // the manager projecting the pin back.
        pins.evaluate(Ok(REBUILT_STORE_PATH));
        for declaration in [paused_nix(2), resumed_nix(3)] {
            node.declare(declaration);
            realizer
                .inner
                .pin_withheld_engines()
                .await
                .expect("pin pass");
            assert_eq!(launch_identity(&input), STORE_PATH);
        }
        drop(realizer);
        let restarted_input = node.pinned_input(&pins);
        let restarted = node.realizer_for(restarted_input.clone());
        let mut projected = resumed_nix(3);
        projected.node_resolved_launch_identities = vec![engine_pin(STORE_PATH)];
        node.declare(projected);
        restarted
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");

        assert_eq!(launch_identity(&restarted_input), STORE_PATH);
        assert_eq!(pins.resolutions(), ["engines.llama-cpp.native"]);
        assert_eq!(node.retained(), [engine_pin(STORE_PATH)]);
    }

    #[tokio::test]
    async fn deployments_of_one_development_engine_share_one_evaluation_per_pass() {
        let node = Node::new();
        node.declare_all(vec![present_nix("deployment", 1), present_nix("second", 1)]);
        let pins = TestPins::evaluating(STORE_PATH);
        let realizer = node.realizer(&pins);

        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");

        assert_eq!(pins.resolutions(), ["engines.llama-cpp.native"]);
        assert_eq!(
            node.retained(),
            [engine_pin(STORE_PATH), engine_pin(STORE_PATH)]
        );
    }

    #[tokio::test]
    async fn a_node_without_a_development_source_fails_the_deployment_naming_the_node() {
        let node = Node::new();
        node.declare(present_nix("deployment", 1));
        let realizer = node.realizer(&TestPins::default());

        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");

        let state = node.state_of("deployment").expect("failed record");
        assert_eq!(
            ModelDeploymentStatus::try_from(state.status),
            Ok(ModelDeploymentStatus::Failed)
        );
        assert_eq!(state.status_message, ENGINE_ARTIFACT_UNAVAILABLE);
        assert_eq!(
            state.failure,
            Some(ModelDeploymentFailure {
                code: LAUNCH_IDENTITY_UNRESOLVED_CODE.to_owned(),
                message: "Node \"node-one\" has no development source to evaluate engine package \"engines.llama-cpp.native\" from; set DOTCORE_DEV_SOURCE_FLAKE on that node, then pause and resume the deployment".to_owned(),
                diagnostics: Vec::new(),
            })
        );
        assert!(node.retained().is_empty());
    }

    #[tokio::test]
    async fn a_failed_evaluation_reports_what_nix_said_and_retries_only_for_a_new_generation() {
        let node = Node::new();
        node.declare(present_nix("deployment", 1));
        let pins = TestPins::default();
        pins.evaluate(Err("error: attribute 'engines.llama-cpp.native' missing"));
        let realizer = node.realizer(&pins);

        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");
        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("failed pass");

        assert_eq!(
            node.state_of("deployment").and_then(|state| state.failure),
            Some(ModelDeploymentFailure {
                code: LAUNCH_IDENTITY_UNRESOLVED_CODE.to_owned(),
                message: "Engine package \"engines.llama-cpp.native\" could not be evaluated from the development source of node \"node-one\"; fix the source on that node, then pause and resume the deployment".to_owned(),
                diagnostics: vec![
                    "Nix reported: error: attribute 'engines.llama-cpp.native' missing".to_owned()
                ],
            })
        );
        assert_eq!(
            pins.resolutions(),
            ["engines.llama-cpp.native"],
            "a failed generation is not retried"
        );

        pins.evaluate(Ok(STORE_PATH));
        node.declare(paused_nix(2));
        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pause pass");

        assert_eq!(pins.resolutions().len(), 2);
        assert_eq!(node.retained(), [engine_pin(STORE_PATH)]);
    }

    #[tokio::test]
    async fn shutting_the_realizer_down_cancels_an_evaluation_in_flight_without_failing() {
        let node = Node::new();
        node.declare(present_nix("deployment", 1));
        let pins = TestPins::evaluating(STORE_PATH);
        pins.hold();
        let realizer = node.realizer(&pins);
        let inner = Arc::clone(&realizer.inner);
        let pass = tokio::spawn(async move { inner.pin_withheld_engines().await });
        pins.started.notified().await;

        node.shutdown.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(5), pass)
            .await
            .expect("the pass returns promptly")
            .expect("pass task")
            .expect("cancelled pass");
        assert!(
            node.state_of("deployment")
                .is_none_or(|state| state.failure.is_none())
        );
        assert!(node.retained().is_empty());
    }

    #[tokio::test]
    async fn a_desired_view_change_cancels_an_evaluation_and_the_next_pass_pins_again() {
        let node = Node::new();
        let mut other = test_desired_deployment("model", ModelDeploymentDesiredState::Present);
        other.model_deployment_id = "other".to_owned();
        node.declare_all(vec![present_nix("deployment", 1), other]);
        let pins = TestPins::evaluating(STORE_PATH);
        pins.hold();
        let realizer = node.realizer(&pins);
        let inner = Arc::clone(&realizer.inner);
        let pass = tokio::spawn(async move { inner.pin_withheld_engines().await });
        pins.started.notified().await;

        // Deleting another deployment from the node's view does not wait for the evaluation.
        node.declare(present_nix("deployment", 1));

        tokio::time::timeout(std::time::Duration::from_secs(5), pass)
            .await
            .expect("the pass returns promptly")
            .expect("pass task")
            .expect("cancelled pass");
        assert!(node.retained().is_empty());
        assert!(
            node.state_of("deployment")
                .is_none_or(|state| state.failure.is_none())
        );

        pins.release();
        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("next pass");

        assert_eq!(
            pins.resolutions(),
            ["engines.llama-cpp.native", "engines.llama-cpp.native"]
        );
        assert_eq!(node.retained(), [engine_pin(STORE_PATH)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_deployment_declared_during_an_evaluation_is_pinned_before_the_safety_scan() {
        // Waits after the cancelled pass pick a ready branch at random, so a lost wakeup shows
        // only over several rounds.
        for round in 0..16 {
            let node = Node::new();
            node.declare(present_nix("deployment", 1));
            let pins = TestPins::evaluating(STORE_PATH);
            pins.hold();
            let realizer = node.realizer(&pins);
            realizer.start_observer();
            pins.started.notified().await;
            let declared = tokio::time::Instant::now();

            pins.release();
            node.declare_all(vec![present_nix("deployment", 1), present_nix("second", 1)]);

            while node.retained().len() < 2 {
                assert!(
                    declared.elapsed()
                        < dotcore_domain::timing::Timing::production()
                            .model_reconcile
                            .settled,
                    "round {round}: both deployments were not pinned before the safety scan: {:?}",
                    node.retained()
                );
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(
                node.retained(),
                [engine_pin(STORE_PATH), engine_pin(STORE_PATH)]
            );
            node.shutdown.cancel();
        }
    }

    #[tokio::test]
    async fn an_unpinned_development_source_teardown_is_never_withheld_for_evaluation() {
        let node = Node::new();
        let mut teardown = present_nix("deployment", 2);
        teardown.desired_state = ModelDeploymentDesiredState::Teardown as i32;
        node.declare(teardown.clone());
        let input = node.pinned_input(&TestPins::evaluating(STORE_PATH));

        assert_eq!(input.model_deployments().expect("deployments"), [teardown]);
        assert!(
            input.withheld_deployments().expect("withheld").is_empty(),
            "the pin pass only evaluates withheld deployments"
        );
    }

    #[tokio::test]
    async fn an_unpinned_teardown_builds_the_node_realizer_and_confirms_cleanup() {
        for deployment in [present(2), present_nix("deployment", 2)] {
            let node = Node::new();
            node.declare(torn_down(deployment));
            let realizer = node.realizer(&TestPins::default());

            realizer.inner.reconcile().await.expect("teardown pass");

            let evidence = node
                .state
                .node_model_cleanup_evidence()
                .expect("cleanup evidence");
            assert_eq!(
                evidence
                    .iter()
                    .map(|evidence| (
                        evidence.model_deployment_id.as_str(),
                        evidence.lifecycle_generation
                    ))
                    .collect::<Vec<_>>(),
                [("deployment", 2)]
            );
            assert!(node.state_of("deployment").is_none());
        }
    }

    #[tokio::test]
    async fn a_teardown_keeps_the_agreed_pin_and_is_never_withheld_by_a_conflict() {
        let node = Node::new();
        node.state
            .retain_node_model_resolved_launch_identities(NodeResolvedLaunchIdentities {
                deployment_id: "deployment".to_owned(),
                resolved_launch_identities: vec![engine_pin(PINNED)],
            })
            .expect("retained pin");
        let input = node.pinned_input(&TestPins::default());

        node.declare(torn_down(present(2)));
        assert_eq!(launch_identity(&input), PINNED);

        let mut contradicted = torn_down(present(3));
        contradicted.node_resolved_launch_identities = vec![engine_pin(REBUILT)];
        node.declare(contradicted.clone());
        assert_eq!(
            input.model_deployments().expect("deployments"),
            [contradicted]
        );
        assert!(input.withheld_deployments().expect("withheld").is_empty());
    }

    #[tokio::test]
    async fn node_logs_of_an_unpinned_teardown_are_not_found() {
        let node = Node::new();
        node.declare(torn_down(present_nix("deployment", 2)));
        let logs = crate::model::controller::NodeModelLogs::new(
            node.pinned_input(&TestPins::default()),
            node.runtimes(),
            dotcore_artifact::ArtifactManager::new(node._directory.path().join("cache"), None)
                .expect("artifact paths"),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
            crate::deployment_logs::PreparationLogs::on_system_clock(node._directory.path()),
            crate::deployment_logs::WorkloadLifecycle::new(),
        );
        use crate::deployment_logs::DeploymentLogOwner as _;

        let result = logs
            .read_logs(
                "deployment",
                crate::management_logs::DeploymentLogReadQuery::new(
                    Vec::new(),
                    dotcore_runtime::RuntimeLogQuery::new(
                        std::num::NonZeroU16::new(1).expect("limit"),
                        None,
                    )
                    .expect("query"),
                ),
            )
            .await;

        assert!(matches!(
            result,
            Err(crate::management_logs::DeploymentLogError::NotFound)
        ));
    }

    #[test]
    fn pin_failures_are_worded_by_cause_and_only_quote_nix_when_it_ran() {
        use dotcore_artifact::NixEvaluationError;

        let selection = UnpinnedSelection {
            selection_key: ENGINE_SELECTION_KEY.to_owned(),
            source: PinSource::NixDevelopmentSource {
                package: "engines.llama-cpp.native".to_owned(),
                docker_image: false,
            },
        };
        let unresolved = |error: NixPinError| {
            // Shared like a resolution reused within one pass.
            let shared = SharedPinError(Arc::new(anyhow::Error::new(error)));
            pin_failure(
                &LaunchPinError::Unresolved {
                    selection_key: ENGINE_SELECTION_KEY.to_owned(),
                    source: anyhow::Error::new(shared),
                },
                &selection,
                "node-one",
            )
        };
        let evaluation = |source| {
            unresolved(NixPinError::Evaluation {
                package: "engines.llama-cpp.native".to_owned(),
                system: "aarch64-darwin",
                source,
            })
        };
        let failed = evaluation(NixEvaluationError::Failed {
            installable: "git+file:///src#legacyPackages.aarch64-darwin.engines.llama-cpp.native"
                .to_owned(),
            stderr: "error: attribute missing".to_owned(),
        });
        let missing = evaluation(NixEvaluationError::NixMissing);
        let timed_out = evaluation(NixEvaluationError::TimedOut {
            installable: "git+file:///src#legacyPackages.aarch64-darwin.engines.llama-cpp.native"
                .to_owned(),
            timeout: std::time::Duration::from_secs(300),
        });
        let invalid = unresolved(NixPinError::InvalidStorePath {
            package: "engines.llama-cpp.native".to_owned(),
            store_path: "/tmp/llama-server".to_owned(),
        });
        let kind_mismatch = pin_failure(
            &LaunchPinError::KindMismatch {
                selection_key: ENGINE_SELECTION_KEY.to_owned(),
            },
            &selection,
            "node-one",
        );

        assert_eq!(
            failed.diagnostics,
            ["Nix reported: error: attribute missing"]
        );
        assert_eq!(
            missing.message,
            "Node \"node-one\" cannot evaluate engine package \"engines.llama-cpp.native\": Nix was not found in /run/current-system/sw/bin, /nix/var/nix/profiles/default/bin, or PATH; install Nix on that node, then pause and resume the deployment"
        );
        assert_eq!(
            timed_out.message,
            "Evaluating engine package \"engines.llama-cpp.native\" on node \"node-one\" did not finish within 300s; check that its development source and inputs are reachable from that node, then pause and resume the deployment"
        );
        assert!(missing.diagnostics.is_empty() && timed_out.diagnostics.is_empty());
        assert_eq!(
            invalid.message,
            "Engine package \"engines.llama-cpp.native\" could not be pinned on node \"node-one\": package \"engines.llama-cpp.native\" evaluated to an invalid store path \"/tmp/llama-server\"; pause and resume the deployment to retry"
        );
        assert_eq!(
            [&failed, &missing, &timed_out, &invalid].map(|failure| failure.code.as_str()),
            [LAUNCH_IDENTITY_UNRESOLVED_CODE; 4]
        );
        assert_eq!(kind_mismatch.code, LAUNCH_IDENTITY_CONFLICT_CODE);
    }

    #[tokio::test]
    async fn a_retained_pin_of_another_kind_fails_as_a_conflict_without_launching() {
        let node = Node::new();
        node.state
            .retain_node_model_resolved_launch_identities(NodeResolvedLaunchIdentities {
                deployment_id: "deployment".to_owned(),
                resolved_launch_identities: vec![engine_pin(PINNED)],
            })
            .expect("retained image pin");
        node.declare(present_nix("deployment", 1));
        let pins = TestPins::evaluating(STORE_PATH);
        let input = node.pinned_input(&pins);
        let realizer = node.realizer_for(input.clone());

        realizer
            .inner
            .pin_withheld_engines()
            .await
            .expect("pin pass");

        assert!(input.model_deployments().expect("deployments").is_empty());
        assert_eq!(
            node.state_of("deployment")
                .and_then(|state| state.failure)
                .map(|failure| failure.code),
            Some(LAUNCH_IDENTITY_CONFLICT_CODE.to_owned())
        );
        assert!(pins.resolutions().is_empty());
    }

    #[tokio::test]
    async fn a_pass_cancelled_by_shutdown_starts_no_further_reconciliation() {
        let node = Node::new();
        let mut other = test_desired_deployment("model", ModelDeploymentDesiredState::Present);
        other.model_deployment_id = "other".to_owned();
        node.declare_all(vec![present_nix("deployment", 1), other]);
        let pins = TestPins::evaluating(STORE_PATH);
        pins.hold();
        let realizer = node.realizer(&pins);
        let record = |id: &str| {
            node.state
                .snapshot()
                .expect("node state")
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == id)
        };
        let before = record("other");
        let inner = Arc::clone(&realizer.inner);
        let pass = tokio::spawn(async move { inner.reconcile().await });
        pins.started.notified().await;

        node.shutdown.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(5), pass)
            .await
            .expect("the pass returns promptly")
            .expect("pass task")
            .expect("shut-down pass");
        assert_eq!(
            record("other"),
            before,
            "the other deployment is left untouched"
        );
        assert!(node.created_images().is_empty());
    }

    #[tokio::test]
    async fn an_invalid_deployment_fails_alone_while_the_node_runs_the_others() {
        let node = Node::new();
        let mut other = test_desired_deployment("model", ModelDeploymentDesiredState::Present);
        other.model_deployment_id = "other".to_owned();
        let mut paused_invalid = without_launch_plan(test_desired_deployment(
            "model",
            ModelDeploymentDesiredState::Paused,
        ));
        paused_invalid.model_deployment_id = "paused".to_owned();
        node.declare_all(vec![
            without_launch_plan(present(1)),
            paused_invalid,
            other.clone(),
        ]);
        let images = TestPins::with(PINNED);
        let realizer = node.realizer(&images);

        reconcile_until_of(&realizer, &node, "other", ModelDeploymentStatus::Ready).await;
        let failed = reconcile_until(&realizer, &node, ModelDeploymentStatus::Failed).await;
        let paused_failure = node.state_of("paused").and_then(|state| state.failure);

        let invalid = |deployment_id: &str| ModelDeploymentFailure {
            code: LAUNCH_INVALID_CODE.to_owned(),
            message: format!(
                "The launch of deployment {deployment_id:?} cannot be realized on node \"node-one\": Invalid catalog: local deployment snapshot is incomplete; tear the deployment down and create it again"
            ),
            diagnostics: vec![
                "Invalid catalog: local deployment snapshot is incomplete".to_owned(),
            ],
        };
        assert_eq!(failed.failure, Some(invalid("deployment")));
        assert_eq!(paused_failure, Some(invalid("paused")));
        assert_eq!(
            node.retained(),
            [engine_pin(PINNED)],
            "the engine is pinned before the launch is validated"
        );
        assert!(
            !node.created_images().iter().any(|image| image == PINNED),
            "the invalid deployment never launches"
        );

        node.declare_all(vec![torn_down(without_launch_plan(present(2))), other]);
        realizer.inner.reconcile().await.expect("teardown pass");

        let evidence = node
            .state
            .node_model_cleanup_evidence()
            .expect("cleanup evidence");
        assert_eq!(
            evidence
                .iter()
                .map(|evidence| (
                    evidence.model_deployment_id.as_str(),
                    evidence.lifecycle_generation
                ))
                .collect::<Vec<_>>(),
            [("deployment", 2)]
        );
        assert!(node.state_of("deployment").is_none());
    }

    #[tokio::test]
    async fn a_running_deployment_whose_launch_becomes_invalid_fails_and_its_workload_is_removed() {
        let node = Node::new();
        let realizer = node.realizer(&TestPins::with(PINNED));
        reconcile_until(&realizer, &node, ModelDeploymentStatus::Ready).await;
        let served = || {
            realizer
                .inner
                .serving_targets_source
                .snapshot()
                .targets
                .into_iter()
                .map(|target| target.deployment_id.0)
                .collect::<Vec<_>>()
        };
        assert_eq!(node.workloads().await.len(), 1);
        assert_eq!(served(), ["deployment"]);

        node.declare(without_launch_plan(present(2)));
        let failed = reconcile_until(&realizer, &node, ModelDeploymentStatus::Failed).await;
        realizer
            .inner
            .reconcile()
            .await
            .expect("orphan cleanup pass");

        assert_eq!(
            failed.failure.map(|failure| failure.code),
            Some(LAUNCH_INVALID_CODE.to_owned())
        );
        assert!(node.workloads().await.is_empty());
        assert!(
            served().is_empty(),
            "the failed deployment is no longer served"
        );
    }

    /// Launch identity of the one deployment the realizer receives from `input`.
    fn launch_identity(input: &NodePinnedModelInput<ProjectedTestInput>) -> String {
        let [declaration] = <[ModelDeploymentDeclaration; 1]>::try_from(
            input.model_deployments().expect("deployments"),
        )
        .expect("one pinned deployment");
        crate::model::resolved::ResolvedModelLaunch::from_declaration(&declaration)
            .expect("pinned launch")
            .engine
            .launch_identity
    }

    async fn reconcile_until(
        realizer: &NodeRealizer,
        node: &Node,
        status: ModelDeploymentStatus,
    ) -> ModelDeploymentState {
        reconcile_until_of(realizer, node, "deployment", status).await
    }

    async fn reconcile_until_of(
        realizer: &NodeRealizer,
        node: &Node,
        deployment_id: &str,
        status: ModelDeploymentStatus,
    ) -> ModelDeploymentState {
        for _ in 0..20 {
            realizer
                .inner
                .reconcile()
                .await
                .expect("reconciliation pass");
            if let Some(state) = node.state_of(deployment_id)
                && ModelDeploymentStatus::try_from(state.status) == Ok(status)
            {
                return state;
            }
        }
        panic!(
            "{deployment_id} never reached {status:?}: {:?}",
            node.state_of(deployment_id)
        );
    }

    type NodeRealizer = ModelRealizer<
        NodePinnedModelInput<ProjectedTestInput>,
        FileNodeState,
        super::super::DurableModelRealizationResults,
    >;

    /// One node with a retained state file and a simulated runtime that records the image
    /// of every workload it creates; both survive realizer restarts.
    struct Node {
        site: Site,
        input: ProjectedTestInput,
        state: FileNodeState,
        runtime: ImageRecordingRuntime,
        shutdown: CancellationToken,
        _directory: tempfile::TempDir,
    }

    impl Node {
        fn new() -> Self {
            let mut site = test_site();
            site.model_deployments = vec![present(1)];
            let input = ProjectedTestInput::from_snapshot(snapshot_of(&site));
            let directory = crate::test_support::canonical_tempdir().expect("state directory");
            let state = FileNodeState::open(
                directory.path().join("node"),
                site.site_id.clone(),
                "node-one",
            )
            .expect("node state");
            Self {
                site,
                input,
                state,
                runtime: ImageRecordingRuntime {
                    runtime: Arc::new(
                        InMemoryRuntimeBackend::new(ExecutionPlatform::linux_amd64()),
                    ),
                    created_images: Arc::new(Mutex::new(Vec::new())),
                    // Runs its image, as Docker does, with no host mounts to prepare.
                    requirements: WorkloadMaterializationRequirements {
                        executable_artifact: WorkloadAssetMaterialization::Physical,
                        host_mounts: WorkloadAssetMaterialization::ReferenceOnly,
                    },
                },
                shutdown: CancellationToken::new(),
                _directory: directory,
            }
        }

        fn realizer(&self, images: &TestPins) -> NodeRealizer {
            self.realizer_for(self.pinned_input(images))
        }

        fn pinned_input(&self, images: &TestPins) -> NodePinnedModelInput<ProjectedTestInput> {
            NodePinnedModelInput::new(
                self.input.clone(),
                self.state.clone(),
                "node-one".to_owned(),
                Arc::new(images.clone()),
            )
            .expect("pinned input")
        }

        fn realizer_for(&self, input: NodePinnedModelInput<ProjectedTestInput>) -> NodeRealizer {
            ModelRealizer::new_node_realizer(
                NodeModelRealizerDependencies {
                    input,
                    composition: ModelControllerDependencies {
                        artifacts: ArtifactDependencies::default(),
                        target_capabilities: test_target_capability_source(),
                        engine_integrations: test_engine_integrations(),
                        accelerator_attribution: None,
                    },
                    serving_targets: ServingTargetsPublisher::new().0,
                    shutdown: self.shutdown.clone(),
                    workload_lifecycle: crate::deployment_logs::WorkloadLifecycle::new(),
                    reconcile: dotcore_domain::timing::Timing::production().model_reconcile,
                    preparation_logs: crate::deployment_logs::PreparationLogs::on_system_clock(
                        self._directory.path(),
                    ),
                },
                self.state.clone(),
                self.runtimes(),
                Vec::new(),
                self._directory.path().join("cache"),
            )
            .expect("node realizer")
        }

        fn runtimes(&self) -> RuntimeTargetMap {
            let mut builder = RuntimeTargetMapBuilder::new();
            builder
                .insert("target-one", self.runtime.clone())
                .expect("Runtime adapter");
            builder.build().expect("Runtime map")
        }

        fn declare(&self, deployment: ModelDeploymentDeclaration) {
            self.declare_all(vec![deployment]);
        }

        fn declare_all(&self, deployments: Vec<ModelDeploymentDeclaration>) {
            let mut site = self.site.clone();
            site.model_deployments = deployments;
            self.input.replace(snapshot_of(&site));
        }

        fn state_of(&self, deployment_id: &str) -> Option<ModelDeploymentState> {
            self.state
                .snapshot()
                .expect("node state")
                .model_deployments
                .into_iter()
                .find(|record| record.model_deployment_id == deployment_id)
                .and_then(|record| record.state)
        }

        fn retained(&self) -> Vec<NodeResolvedLaunchIdentity> {
            self.state
                .node_model_resolved_launch_identities()
                .expect("retained pins")
                .into_iter()
                .flat_map(|record| record.resolved_launch_identities)
                .collect()
        }

        fn created_images(&self) -> Vec<String> {
            self.runtime.created_images.lock().expect("images").clone()
        }

        async fn workloads(&self) -> Vec<WorkloadObservation> {
            self.runtime
                .discover_workloads()
                .await
                .expect("discovered workloads")
        }
    }

    fn snapshot_of(site: &Site) -> ModelSiteSnapshot {
        ModelSiteSnapshot {
            site_id: site.site_id.clone(),
            execution_targets: site.execution_targets.clone(),
            deployments: site.model_deployments.clone(),
        }
    }

    fn present(lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        let mut deployment = test_desired_deployment("model", ModelDeploymentDesiredState::Present);
        crate::test_support::retain_tag_only_engine(
            deployment.snapshot.as_mut().expect("snapshot"),
            "",
        );
        deployment.lifecycle_generation = lifecycle_generation;
        deployment
    }

    fn paused(lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            desired_state: ModelDeploymentDesiredState::Paused as i32,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Pause as i32,
            ..present(lifecycle_generation)
        }
    }

    fn resumed(lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            lifecycle_transition: ModelDeploymentLifecycleTransition::Resume as i32,
            ..present(lifecycle_generation)
        }
    }

    /// A present deployment whose engine is package `engines.llama-cpp.native` of the node's
    /// development source, left for this node to pin.
    fn present_nix(deployment_id: &str, lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        let mut deployment = present(lifecycle_generation);
        deployment.model_deployment_id = deployment_id.to_owned();
        crate::test_support::nix::retain_development_source_engine(
            deployment.snapshot.as_mut().expect("snapshot"),
            "",
        );
        deployment
    }

    fn paused_nix(lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            desired_state: ModelDeploymentDesiredState::Paused as i32,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Pause as i32,
            ..present_nix("deployment", lifecycle_generation)
        }
    }

    fn resumed_nix(lifecycle_generation: u64) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            lifecycle_transition: ModelDeploymentLifecycleTransition::Resume as i32,
            ..present_nix("deployment", lifecycle_generation)
        }
    }

    fn torn_down(deployment: ModelDeploymentDeclaration) -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            desired_state: ModelDeploymentDesiredState::Teardown as i32,
            lifecycle_transition: ModelDeploymentLifecycleTransition::Teardown as i32,
            ..deployment
        }
    }

    /// `deployment` without its retained launch plan, so its launch no longer resolves.
    fn without_launch_plan(
        mut deployment: ModelDeploymentDeclaration,
    ) -> ModelDeploymentDeclaration {
        let Some(dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source::Local(
            local,
        )) = deployment
            .snapshot
            .as_mut()
            .and_then(|snapshot| snapshot.source.as_mut())
        else {
            panic!("local snapshot");
        };
        local.desired_launch_plan = None;
        deployment
    }

    fn engine_pin(launch_identity: &str) -> NodeResolvedLaunchIdentity {
        NodeResolvedLaunchIdentity {
            selection_key: ENGINE_SELECTION_KEY.to_owned(),
            launch_identity: launch_identity.to_owned(),
        }
    }

    /// Node fake of the image store and the development source: the tag names at most one image,
    /// the development source evaluates to a store path or fails, an evaluation may be held until
    /// cancelled, and every resolution is recorded by its tag or package.
    #[derive(Clone, Default)]
    struct TestPins {
        tagged: Arc<Mutex<Option<String>>>,
        evaluated: Arc<Mutex<Option<Result<String, String>>>>,
        held: Arc<std::sync::atomic::AtomicBool>,
        started: Arc<Notify>,
        resolutions: Arc<Mutex<Vec<String>>>,
    }

    impl TestPins {
        fn with(image_id: &str) -> Self {
            let images = Self::default();
            images.retag(image_id);
            images
        }

        fn evaluating(store_path: &str) -> Self {
            let pins = Self::default();
            pins.evaluate(Ok(store_path));
            pins
        }

        fn retag(&self, image_id: &str) {
            *self.tagged.lock().expect("tag") = Some(image_id.to_owned());
        }

        /// Sets what the development source evaluates to: a store path, or Nix's stderr.
        fn evaluate(&self, outcome: Result<&str, &str>) {
            *self.evaluated.lock().expect("evaluation") =
                Some(outcome.map(str::to_owned).map_err(str::to_owned));
        }

        /// Holds every later resolution until its cancellation fires.
        fn hold(&self) {
            self.held.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        fn release(&self) {
            self.held.store(false, std::sync::atomic::Ordering::SeqCst);
        }

        fn resolutions(&self) -> Vec<String> {
            self.resolutions.lock().expect("resolutions").clone()
        }
    }

    impl LaunchPinResolver for TestPins {
        fn resolve_pin<'a>(
            &'a self,
            source: &'a PinSource,
            _platform: &'a OciPlatform,
            cancellation: CancellationToken,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + 'a>> {
            Box::pin(async move {
                let label = match source {
                    PinSource::LocalImage { reference } => reference.clone(),
                    PinSource::NixDevelopmentSource { package, .. } => package.clone(),
                    PinSource::NixLocked { store_paths } => format!("{store_paths:?}"),
                };
                self.resolutions
                    .lock()
                    .expect("resolutions")
                    .push(label.clone());
                if self.held.load(std::sync::atomic::Ordering::SeqCst) {
                    self.started.notify_one();
                    cancellation.cancelled().await;
                    return Err(anyhow::Error::new(NixPinError::Evaluation {
                        package: label.clone(),
                        system: "x86_64-linux",
                        source: dotcore_artifact::NixEvaluationError::Cancelled,
                    }));
                }
                match source {
                    PinSource::LocalImage { reference } => {
                        self.tagged.lock().expect("tag").clone().ok_or_else(|| {
                            anyhow::anyhow!("no local image is tagged {reference:?}")
                        })
                    }
                    PinSource::NixDevelopmentSource { package, .. } => {
                        match self.evaluated.lock().expect("evaluation").clone() {
                            Some(Ok(store_path)) => Ok(store_path),
                            Some(Err(stderr)) => Err(anyhow::Error::new(NixPinError::Evaluation {
                                package: package.clone(),
                                system: "x86_64-linux",
                                source: dotcore_artifact::NixEvaluationError::Failed {
                                    installable: format!("git+file:///src#{package}"),
                                    stderr,
                                },
                            })),
                            None => Err(anyhow::Error::new(
                                NixPinError::DevelopmentSourceUnavailable {
                                    package: package.clone(),
                                },
                            )),
                        }
                    }
                    PinSource::NixLocked { store_paths } => store_paths
                        .first()
                        .map(|(_, store_path)| store_path.clone())
                        .ok_or_else(|| {
                            anyhow::Error::new(NixPinError::StorePathUnavailable {
                                system: "x86_64-linux",
                            })
                        }),
                }
            })
        }
    }

    /// Simulated runtime that records the image of each created workload, reporting the
    /// materialization requirements of the runtime it stands for.
    #[derive(Clone)]
    struct ImageRecordingRuntime {
        runtime: Arc<InMemoryRuntimeBackend>,
        created_images: Arc<Mutex<Vec<String>>>,
        requirements: WorkloadMaterializationRequirements,
    }

    impl RuntimeBackend for ImageRecordingRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            self.runtime.backend_id()
        }

        fn execution_platform<'a>(&'a self) -> RuntimeFuture<'a, ExecutionPlatform> {
            self.runtime.execution_platform()
        }

        fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
            self.requirements
        }

        fn prepare_realization<'a>(
            &'a self,
            request: &'a WorkloadStartupRequest,
            cancellation: tokio_util::sync::CancellationToken,
            observer: Option<WorkloadStartupObserver>,
        ) -> RuntimeFuture<'a, ()> {
            self.runtime
                .prepare_realization(request, cancellation, observer)
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            self.runtime.discover_workloads()
        }

        fn create_workload<'a>(
            &'a self,
            workload: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            self.created_images
                .lock()
                .expect("images")
                .push(workload.spec.executable.artifact.reference().to_owned());
            self.runtime.create_workload(workload)
        }

        fn start_workload<'a>(
            &'a self,
            workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            self.runtime.start_workload(workload_id)
        }

        fn inspect_workload<'a>(
            &'a self,
            workload_id: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            self.runtime.inspect_workload(workload_id)
        }

        fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            self.runtime.stop_workload(workload_id)
        }

        fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            self.runtime.remove_workload(workload_id)
        }

        fn read_workload_logs_by_correlation<'a>(
            &'a self,
            correlation: &'a dotcore_runtime::WorkloadCorrelation,
            query: &'a dotcore_runtime::RuntimeWorkloadLogQuery,
        ) -> dotcore_runtime::RuntimeLogFuture<'a, dotcore_runtime::RuntimeLogHistory> {
            self.runtime
                .read_workload_logs_by_correlation(correlation, query)
        }
    }
}
