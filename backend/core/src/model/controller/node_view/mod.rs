//! Node inference-controller section of the status reports a SiteNode sends for its installed
//! desired view.
//!
//! Each report describes every installed model assignment from the node record that reconciled
//! its exact lifecycle generation, the live serving view, and the node's own accelerator
//! observations, with unacknowledged cleanup evidence standing in for its teardown. Reported
//! values are cut to the site's status contract sizes.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use dotcore_proto::dotcore::{
    deployment::v1::{AcceleratorMemoryObservation, NodeResolvedLaunchIdentity},
    model::{
        private::v1::{
            ModelBindingReadiness, ModelControllerStatusSection,
            ModelDeploymentStatus as PrivateModelDeploymentStatus, ModelLifecycleObservation,
            ModelPreparationProgress,
        },
        v1::{
            CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentDesiredState,
            ModelDeploymentProgress, ModelDeploymentProgressUnit, ModelDeploymentStatus,
        },
    },
    site::{store::v1::NodeModelCleanupEvidence, v1::ExecutionTarget},
};
use dotcore_site::{
    node::hardware_inventory::HardwareCollection,
    repository::{ModelSiteSnapshot, SiteRepositoryError},
    store::{FileNodeState, StoreError},
    view::{
        contract::{
            bounded_accelerator_memory, bounded_failure, bounded_observed_placements,
            bounded_status_message, cleanup_evidence_from_state,
        },
        desired::NodeModelStatusOwner,
        fencing::AcceptedModelOwnerInput,
    },
};
use tokio::sync::Notify;
use tracing::warn;

use super::{
    AcceleratorAttribution, ModelRealizationInput,
    metrics::HostedAcceleratorMemory,
    serving_targets::{LocalServingTarget, LocalServingTargetsSource},
};
use crate::model::state::{CurrentStateRepository, CurrentStateSnapshot};

/// Model realization input backed by the installed view's model section.
#[derive(Clone)]
pub(crate) struct NodeViewModelInput {
    input: AcceptedModelOwnerInput,
}

impl NodeViewModelInput {
    pub(crate) fn new(input: AcceptedModelOwnerInput) -> Self {
        Self { input }
    }
}

impl ModelRealizationInput for NodeViewModelInput {
    fn declaration_snapshot(&self) -> Result<ModelSiteSnapshot, SiteRepositoryError> {
        self.input.declaration_snapshot()
    }

    fn model_deployments(&self) -> Result<Vec<ModelDeploymentDeclaration>, SiteRepositoryError> {
        self.declaration_snapshot()
            .map(|snapshot| snapshot.deployments)
    }

    fn register_change_listener(&self) -> Arc<Notify> {
        self.input.register_change_listener()
    }
}

/// Builds the model section of one SiteNode's status reports from its installed view, its node
/// store, its live serving targets, and its accelerator observations.
pub(crate) struct NodeModelStatus {
    node_id: String,
    projected: AcceptedModelOwnerInput,
    state: FileNodeState,
    serving_targets: LocalServingTargetsSource,

    /// Attributes accelerator memory to each reported deployment from the node's own
    /// observations, sharing the identities the node's model realizer records.
    accelerator_memory: AcceleratorAttribution,
}

impl NodeModelStatus {
    pub(crate) fn new(
        node_id: impl Into<String>,
        projected: AcceptedModelOwnerInput,
        state: FileNodeState,
        serving_targets: LocalServingTargetsSource,
        accelerator_memory: AcceleratorAttribution,
    ) -> Self {
        Self {
            node_id: node_id.into(),
            projected,
            state,
            serving_targets,
            accelerator_memory,
        }
    }

    /// Reads the installed view, the node records, and the unacknowledged cleanup evidence a
    /// status is built from; a failed node-local read is logged and reads as none, and the next
    /// poll tries again.
    fn installed(&self) -> Option<InstalledAssignments> {
        let snapshot = self
            .projected
            .declaration_snapshot()
            .inspect_err(|error| warn_status_not_built("installed view", error))
            .ok()?;
        let current = CurrentStateRepository::snapshot(&self.state)
            .inspect_err(|error| warn_status_not_built("current state", error))
            .ok()?;
        let evidence = self
            .state
            .node_model_cleanup_evidence()
            .inspect_err(|error| warn_status_not_built("cleanup evidence", error))
            .ok()?
            .into_iter()
            .filter(|evidence| !evidence.acknowledged)
            .collect();
        Some(InstalledAssignments {
            snapshot,
            current,
            evidence,
        })
    }

    /// Attributes the deployment's accelerator memory from one of the node's own collections,
    /// against its record only when that record observes the assigned generation.
    fn accelerator_memory_status(
        &self,
        declaration: &ModelDeploymentDeclaration,
        record: Option<&CurrentModelDeployment>,
        targets: &[ExecutionTarget],
        collection: &HardwareCollection,
    ) -> AcceleratorMemoryObservation {
        let state = record
            .filter(|record| {
                record.reconciled_lifecycle_generation == Some(declaration.lifecycle_generation)
            })
            .and_then(|record| record.state.clone())
            .unwrap_or_default();
        let target = targets.iter().find(|target| {
            declaration.placement.as_ref().is_some_and(|placement| {
                placement.execution_target_id == target.execution_target_id
            })
        });
        bounded_accelerator_memory(
            &self.node_id,
            self.accelerator_memory
                .projection(declaration, &state, target, collection),
        )
    }
}

impl NodeModelStatusOwner for NodeModelStatus {
    /// Builds one status per installed assignment and lists unacknowledged cleanup evidence.
    /// Unacknowledged evidence stands in for its teardown; every other assignment, including a
    /// teardown whose evidence was already acknowledged, reports its own status.
    fn status_section(
        &self,
        collection: &HardwareCollection,
    ) -> Option<ModelControllerStatusSection> {
        let installed = self.installed()?;
        let retained_pins = self
            .state
            .node_model_resolved_launch_identities()
            .inspect_err(|error| warn_status_not_built("resolved launch identities", error))
            .ok()?;
        let retained_pins = retained_pins
            .iter()
            .map(|record| {
                (
                    record.deployment_id.as_str(),
                    record.resolved_launch_identities.as_slice(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let serving = self.serving_targets.snapshot();
        let live_targets = serving
            .targets
            .iter()
            .map(|target| (target.deployment_id.0.as_str(), target))
            .collect::<BTreeMap<_, _>>();
        let deployments = installed
            .reported()
            .map(|(declaration, record)| {
                let mut status = deployment_status(
                    &self.node_id,
                    declaration,
                    record,
                    live_targets
                        .get(declaration.model_deployment_id.as_str())
                        .copied(),
                );
                status.accelerator_memory = Some(self.accelerator_memory_status(
                    declaration,
                    record,
                    &installed.snapshot.execution_targets,
                    collection,
                ));
                status.resolved_launch_identities = unprojected_pins(
                    declaration,
                    retained_pins
                        .get(declaration.model_deployment_id.as_str())
                        .copied()
                        .unwrap_or_default(),
                );
                status
            })
            .collect();
        Some(ModelControllerStatusSection {
            deployments,
            cleanup_evidence: installed
                .evidence
                .iter()
                .map(cleanup_evidence_from_state)
                .collect(),
        })
    }

    fn accept_receipts(&self, acknowledged: &BTreeSet<(String, u64)>) -> Result<(), StoreError> {
        self.state.acknowledge_node_cleanup_evidence(acknowledged)
    }

    fn retire_withdrawn_cleanup_evidence(&self) {
        let Ok(snapshot) = self.projected.declaration_snapshot() else {
            return;
        };
        let installed = snapshot
            .deployments
            .iter()
            .filter(|declaration| {
                declaration.desired_state == ModelDeploymentDesiredState::Teardown as i32
            })
            .map(|declaration| {
                (
                    declaration.model_deployment_id.clone(),
                    declaration.lifecycle_generation,
                )
            })
            .collect();
        if let Err(error) = self.state.retire_acknowledged_cleanup_evidence(&installed) {
            warn!(event_kind = "node_model_cleanup_retirement", outcome = "failed", error = %error, "Node could not retire acknowledged cleanup evidence");
        }
    }

    fn register_change_observer(&self, observer: Arc<dyn Fn() + Send + Sync>) {
        self.serving_targets.register_change_observer(observer);
    }
}

impl HostedAcceleratorMemory for NodeModelStatus {
    /// Attributes accelerator memory to every assignment the node reports, exactly as its status
    /// reports it, all from one collection.
    fn hosted_accelerator_memory(&self) -> Vec<(String, AcceleratorMemoryObservation)> {
        let Some(installed) = self.installed() else {
            return Vec::new();
        };
        let collection = self.accelerator_memory.collection();
        installed
            .reported()
            .map(|(declaration, record)| {
                (
                    declaration.model_deployment_id.clone(),
                    self.accelerator_memory_status(
                        declaration,
                        record,
                        &installed.snapshot.execution_targets,
                        &collection,
                    ),
                )
            })
            .collect()
    }
}

/// The installed view, node records, and unacknowledged cleanup evidence of one status read.
struct InstalledAssignments {
    snapshot: ModelSiteSnapshot,
    current: CurrentStateSnapshot,
    evidence: Vec<NodeModelCleanupEvidence>,
}

impl InstalledAssignments {
    /// Installed assignments the node reports, each with its node record; an assignment whose
    /// unacknowledged cleanup evidence stands in for it is left out.
    fn reported(
        &self,
    ) -> impl Iterator<Item = (&ModelDeploymentDeclaration, Option<&CurrentModelDeployment>)> {
        let cleaned = self
            .evidence
            .iter()
            .map(|evidence| {
                (
                    evidence.model_deployment_id.as_str(),
                    evidence.lifecycle_generation,
                )
            })
            .collect::<BTreeSet<_>>();
        let records = self
            .current
            .model_deployments
            .iter()
            .map(|record| (record.model_deployment_id.as_str(), record))
            .collect::<BTreeMap<_, _>>();
        self.snapshot
            .deployments
            .iter()
            .filter(move |declaration| {
                !cleaned.contains(&(
                    declaration.model_deployment_id.as_str(),
                    declaration.lifecycle_generation,
                ))
            })
            .map(move |declaration| {
                (
                    declaration,
                    records
                        .get(declaration.model_deployment_id.as_str())
                        .copied(),
                )
            })
    }
}

/// Reports one assignment from the node record that reconciled its exact lifecycle generation.
/// A missing record, an unrecognized status, or a record from another generation reports
/// Reconciling, so an earlier generation's observation never stands in for the assigned one.
///
/// Binding readiness comes from the node's live serving view, not from the record: only a
/// published local target serving the declared lifecycle generation makes a Ready record report a
/// Ready binding, its token, and its runtime reconcile generation.
///
/// The record's lifecycle, status message, failure summary, start steps, and Downloading byte
/// progress are reported as recorded, cut to the contract sizes, and its observed placements keep
/// only the rows the manager accepts from `node_id`. The realizer removes protected launch values before
/// it records any text, so none can leave the node here.
pub(crate) fn deployment_status(
    node_id: &str,
    declaration: &ModelDeploymentDeclaration,
    record: Option<&CurrentModelDeployment>,
    live_target: Option<&LocalServingTarget>,
) -> PrivateModelDeploymentStatus {
    let observation = record
        .filter(|record| {
            record.reconciled_lifecycle_generation == Some(declaration.lifecycle_generation)
        })
        .and_then(|record| {
            let state = record.state.as_ref()?;
            Some((state, lifecycle_observation(state.status)?))
        });
    let mut status = PrivateModelDeploymentStatus {
        model_deployment_id: declaration.model_deployment_id.clone(),
        lifecycle_generation: declaration.lifecycle_generation,
        placement: declaration.placement.clone(),
        realization_generation: None,
        lifecycle: ModelLifecycleObservation::Reconciling as i32,
        status_message: String::new(),
        preparation_progress: None,
        binding_readiness: ModelBindingReadiness::Absent as i32,
        failure: None,
        state_revision: record.map_or(0, |record| record.record_revision),
        observed_placements: Vec::new(),
        binding_token: String::new(),
        resolved_launch_identities: Vec::new(),
        accelerator_memory: None,
        steps: None,
    };
    let Some((state, lifecycle)) = observation else {
        return status;
    };

    let live_target = live_target
        .filter(|target| target.lifecycle_generation == declaration.lifecycle_generation);
    let binding_readiness = match (lifecycle, live_target) {
        (ModelLifecycleObservation::Ready, Some(target)) => {
            status.binding_token = target.binding_token();
            ModelBindingReadiness::Ready
        }
        (
            ModelLifecycleObservation::Loading
            | ModelLifecycleObservation::WarmingUp
            | ModelLifecycleObservation::Ready
            | ModelLifecycleObservation::Unavailable
            | ModelLifecycleObservation::Pausing
            | ModelLifecycleObservation::Paused,
            _,
        ) => ModelBindingReadiness::NotReady,
        _ => ModelBindingReadiness::Absent,
    };
    // A Failed record always reports a failure summary, even one recorded without it.
    let failure = (lifecycle == ModelLifecycleObservation::Failed).then(|| {
        state
            .failure
            .as_ref()
            .map_or_else(Default::default, bounded_failure)
    });
    status.realization_generation = live_target.map(|target| target.reconcile_generation);
    status.lifecycle = lifecycle as i32;
    status.status_message = bounded_status_message(&state.status_message);
    status.preparation_progress = state
        .progress
        .as_ref()
        .filter(|_| lifecycle == ModelLifecycleObservation::Downloading)
        .and_then(preparation_progress);
    status.binding_readiness = binding_readiness as i32;
    status.failure = failure;
    status.observed_placements = bounded_observed_placements(node_id, &state.observed_placements);
    status.steps = state.steps.clone();
    status
}

/// Returns the launch identities this node resolved for the deployment that the manager has
/// not projected back yet; once projected, the node stops reporting them.
fn unprojected_pins(
    declaration: &ModelDeploymentDeclaration,
    retained: &[NodeResolvedLaunchIdentity],
) -> Vec<NodeResolvedLaunchIdentity> {
    retained
        .iter()
        .filter(|pin| !declaration.node_resolved_launch_identities.contains(pin))
        .cloned()
        .collect()
}

/// Reports local byte progress within the contract bounds; a zero total, or one below the
/// completed bytes, is reported as unknown.
fn preparation_progress(progress: &ModelDeploymentProgress) -> Option<ModelPreparationProgress> {
    (progress.unit == ModelDeploymentProgressUnit::Bytes as i32).then(|| ModelPreparationProgress {
        completed_bytes: progress.completed,
        total_bytes: progress
            .total
            .filter(|total| *total != 0 && *total >= progress.completed),
    })
}

/// The lifecycle observation of one recorded status; an unspecified status has none.
fn lifecycle_observation(status: i32) -> Option<ModelLifecycleObservation> {
    match ModelDeploymentStatus::try_from(status).ok()? {
        ModelDeploymentStatus::Queued => Some(ModelLifecycleObservation::Queued),
        ModelDeploymentStatus::Preparing => Some(ModelLifecycleObservation::Preparing),
        ModelDeploymentStatus::Downloading => Some(ModelLifecycleObservation::Downloading),
        ModelDeploymentStatus::Loading => Some(ModelLifecycleObservation::Loading),
        ModelDeploymentStatus::WarmingUp => Some(ModelLifecycleObservation::WarmingUp),
        ModelDeploymentStatus::Ready => Some(ModelLifecycleObservation::Ready),
        ModelDeploymentStatus::Unavailable => Some(ModelLifecycleObservation::Unavailable),
        ModelDeploymentStatus::Pausing => Some(ModelLifecycleObservation::Pausing),
        ModelDeploymentStatus::Paused => Some(ModelLifecycleObservation::Paused),
        ModelDeploymentStatus::Unloading => Some(ModelLifecycleObservation::Stopping),
        ModelDeploymentStatus::Failed => Some(ModelLifecycleObservation::Failed),
        ModelDeploymentStatus::Unspecified => None,
    }
}

/// Reports a node-local read that prevented building a status; the next poll tries again.
fn warn_status_not_built(source: &'static str, error: &impl std::fmt::Display) {
    warn!(event_kind = "node_model_status", outcome = "not_built", source, error = %error, "Node could not build model status; it will retry after the next successful poll");
}

#[cfg(test)]
mod tests;
