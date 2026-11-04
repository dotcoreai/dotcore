//! Inference-controller current-state port for deployment observations.

use std::collections::BTreeSet;

use anyhow::anyhow;
use dotcore_domain::{SiteNodeId, valid_id};
use dotcore_proto::dotcore::model::v1::{
    CurrentModelDeployment, ModelDeploymentProgress, ModelDeploymentProgressUnit,
    ModelDeploymentStatus,
};
use dotcore_site::{
    repository::SiteRepositoryError,
    store::{CurrentStateStore, StoreError, StoredCurrentState},
};

/// A controller-owned current-state read view.
///
/// It contains no desired site or catalog records.
#[derive(Clone, Debug, PartialEq)]
pub struct CurrentStateSnapshot {
    pub site_id: String,
    pub storage_revision: u64,
    pub model_deployments: Vec<CurrentModelDeployment>,
}

/// The revision assigned to one controller-owned current-state record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CurrentStateMutation {
    pub storage_revision: u64,
    pub record_revision: u64,
}

/// Controller-facing current-state port.
///
/// Site-local storage adapters implement this port without changing controller code.
pub trait CurrentStateRepository: Send + Sync {
    fn snapshot(&self) -> Result<CurrentStateSnapshot, CurrentStateError>;
    /// Atomically prepares deployment records during controller construction.
    ///
    /// The expected revision fences a stale construction view. Implementations persist
    /// the complete resulting state as one replacement.
    fn prepare(
        &self,
        deployments: Vec<CurrentModelDeployment>,
        expected_storage_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError>;
    fn upsert(
        &self,
        deployment: CurrentModelDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<CurrentStateMutation, CurrentStateError>;
    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError>;
}

/// Every site store and node store serves the model records it holds; each mutation validates
/// them before the store commits it.
impl<S: CurrentStateStore> CurrentStateRepository for S {
    fn snapshot(&self) -> Result<CurrentStateSnapshot, CurrentStateError> {
        self.read(snapshot_view)?
    }

    fn prepare(
        &self,
        deployments: Vec<CurrentModelDeployment>,
        expected_storage_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.transact(|state| {
            let mutation = prepare_state(state, deployments, expected_storage_revision)?;
            validate_state(state)?;
            Ok(mutation)
        })
    }

    fn upsert(
        &self,
        deployment: CurrentModelDeployment,
        expected_record_revision: Option<u64>,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.transact(|state| {
            let mutation = upsert_state(state, deployment, expected_record_revision)?;
            validate_state(state)?;
            Ok(mutation)
        })
    }

    fn remove(
        &self,
        deployment_id: &str,
        expected_record_revision: u64,
    ) -> Result<CurrentStateMutation, CurrentStateError> {
        self.transact(|state| {
            let mutation = remove_state(state, deployment_id, expected_record_revision)?;
            validate_state(state)?;
            Ok(mutation)
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CurrentStateError {
    #[error("desired site failure")]
    SiteInput(#[source] SiteRepositoryError),

    #[error("current state is invalid")]
    InvalidState,

    #[error("current-state deployment already exists")]
    AlreadyExists,

    #[error("current-state deployment was not found")]
    NotFound,

    #[error("current-state revision does not match")]
    RevisionConflict,

    #[error("current-state revision is exhausted")]
    RevisionExhausted,

    #[error("current state protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("current state is corrupt: {0}")]
    Corrupt(#[source] anyhow::Error),

    #[error("current-state format is unsupported")]
    UnsupportedFormat,

    #[error("current-state site ID does not match")]
    SiteMismatch,

    #[error("current state exceeds the storage limit")]
    LimitExceeded,

    #[error("Current state storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("Current state storage outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),

    #[error("current-state durability is uncertain")]
    Poisoned,

    #[error("current state is unavailable")]
    Unavailable,
}

impl From<StoreError> for CurrentStateError {
    fn from(error: StoreError) -> Self {
        match error {
            StoreError::SiteInput(error) => Self::SiteInput(error),
            StoreError::InvalidState => Self::InvalidState,
            StoreError::RevisionExhausted => Self::RevisionExhausted,
            StoreError::Decode(error) => Self::Decode(error),
            StoreError::Corrupt(error) => Self::Corrupt(error),
            StoreError::UnsupportedFormat => Self::UnsupportedFormat,
            StoreError::SiteMismatch => Self::SiteMismatch,
            StoreError::LimitExceeded => Self::LimitExceeded,
            StoreError::Storage(error) => Self::Storage(error),
            StoreError::OutcomeUncertain(error) => Self::OutcomeUncertain(error),
            StoreError::Poisoned => Self::Poisoned,
            StoreError::Unavailable => Self::Unavailable,
        }
    }
}

impl CurrentStateError {
    pub(crate) fn next_revision(value: u64) -> Result<u64, Self> {
        value.checked_add(1).ok_or(Self::RevisionExhausted)
    }
}

pub(crate) fn snapshot_view<S: StoredCurrentState>(
    state: &S,
) -> Result<CurrentStateSnapshot, CurrentStateError> {
    state.validate_stored().map_err(CurrentStateError::from)?;
    validate_state_deployments(state)?;
    Ok(CurrentStateSnapshot {
        site_id: state.site_id().to_owned(),
        storage_revision: state.storage_revision(),
        model_deployments: state.model_deployments().to_vec(),
    })
}

pub(crate) fn upsert_state<S: StoredCurrentState>(
    state: &mut S,
    mut deployment: CurrentModelDeployment,
    expected: Option<u64>,
) -> Result<CurrentStateMutation, CurrentStateError> {
    validate_deployment(&deployment, false)?;
    let records = state.model_deployments_mut();
    let position = records
        .iter()
        .position(|current| current.model_deployment_id == deployment.model_deployment_id);
    let record_revision = match (position, expected) {
        (None, None) => 1,
        (Some(index), Some(expected)) if records[index].record_revision == expected => {
            CurrentStateError::next_revision(expected)?
        }
        (Some(_), None) => return Err(CurrentStateError::AlreadyExists),
        (None, Some(_)) => return Err(CurrentStateError::NotFound),
        (Some(_), Some(_)) => return Err(CurrentStateError::RevisionConflict),
    };
    deployment.record_revision = record_revision;
    match position {
        Some(index) => records[index] = deployment,
        None => records.push(deployment),
    }
    sort_deployments(records);
    let storage_revision = advance_storage_revision(state)?;
    Ok(CurrentStateMutation {
        storage_revision,
        record_revision,
    })
}

pub(crate) fn prepare_state<S: StoredCurrentState>(
    state: &mut S,
    mut deployments: Vec<CurrentModelDeployment>,
    expected_storage_revision: u64,
) -> Result<CurrentStateMutation, CurrentStateError> {
    if state.storage_revision() != expected_storage_revision {
        return Err(CurrentStateError::RevisionConflict);
    }
    let records = state.model_deployments_mut();
    let mut ids = BTreeSet::new();
    for deployment in &mut deployments {
        validate_deployment(deployment, false)?;
        if !ids.insert(&deployment.model_deployment_id) {
            return Err(CurrentStateError::AlreadyExists);
        }
        if let Some(current) = records
            .iter()
            .find(|current| current.model_deployment_id == deployment.model_deployment_id)
        {
            deployment.record_revision = CurrentStateError::next_revision(current.record_revision)?;
        } else {
            deployment.record_revision = 1;
        }
    }
    for deployment in deployments {
        if let Some(index) = records
            .iter()
            .position(|current| current.model_deployment_id == deployment.model_deployment_id)
        {
            records[index] = deployment;
        } else {
            records.push(deployment);
        }
    }
    sort_deployments(records);
    let storage_revision = advance_storage_revision(state)?;
    Ok(CurrentStateMutation {
        storage_revision,
        record_revision: 0,
    })
}

pub(crate) fn remove_state<S: StoredCurrentState>(
    state: &mut S,
    deployment_id: &str,
    expected: u64,
) -> Result<CurrentStateMutation, CurrentStateError> {
    let records = state.model_deployments_mut();
    let index = records
        .iter()
        .position(|current| current.model_deployment_id == deployment_id)
        .ok_or(CurrentStateError::NotFound)?;
    if records[index].record_revision != expected {
        return Err(CurrentStateError::RevisionConflict);
    }
    records.remove(index);
    let storage_revision = advance_storage_revision(state)?;
    Ok(CurrentStateMutation {
        storage_revision,
        record_revision: expected,
    })
}

pub(crate) fn validate_state<S: StoredCurrentState>(state: &S) -> Result<(), CurrentStateError> {
    validate_state_deployments(state)?;
    Ok(())
}

fn advance_storage_revision<S: StoredCurrentState>(
    state: &mut S,
) -> Result<u64, CurrentStateError> {
    let revision = state.storage_revision_mut();
    *revision = CurrentStateError::next_revision(*revision)?;
    Ok(*revision)
}

fn sort_deployments(records: &mut [CurrentModelDeployment]) {
    records.sort_by(|left, right| left.model_deployment_id.cmp(&right.model_deployment_id));
}

fn validate_state_deployments<S: StoredCurrentState>(state: &S) -> Result<(), CurrentStateError> {
    let mut ids = BTreeSet::new();
    for deployment in state.model_deployments() {
        validate_deployment(deployment, true)?;
        if !ids.insert(&deployment.model_deployment_id) {
            return Err(CurrentStateError::Corrupt(anyhow!(
                "duplicate current-state deployment identity"
            )));
        }
    }
    Ok(())
}

fn validate_deployment(
    deployment: &CurrentModelDeployment,
    persisted: bool,
) -> Result<(), CurrentStateError> {
    let state = deployment
        .state
        .as_ref()
        .ok_or(CurrentStateError::InvalidState)?;
    if !valid_id(&deployment.model_deployment_id)
        || (persisted && deployment.record_revision == 0)
        || ModelDeploymentStatus::try_from(state.status)
            .ok()
            .is_none_or(|value| value == ModelDeploymentStatus::Unspecified)
        || state.status_message.trim().is_empty()
        || state.progress.as_ref().is_some_and(invalid_progress)
        || state.observed_placements.iter().any(|placement| {
            SiteNodeId::parse(&placement.managed_node_id).is_err()
                || placement
                    .accelerator_device_ids
                    .iter()
                    .any(|id| id.trim().is_empty())
        })
    {
        return Err(CurrentStateError::InvalidState);
    }
    Ok(())
}

fn invalid_progress(progress: &ModelDeploymentProgress) -> bool {
    let unit = ModelDeploymentProgressUnit::try_from(progress.unit).ok();
    if unit.is_none_or(|value| value == ModelDeploymentProgressUnit::Unspecified) {
        return true;
    }

    match progress.total {
        None => progress.percent.is_some(),
        Some(0) => true,
        Some(total) => {
            if progress.completed > total {
                return true;
            }
            let derived = ((u128::from(progress.completed) * 100) / u128::from(total)) as u32;
            progress.percent != Some(derived)
        }
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::{ModelDeploymentState, ObservedPlacement};
    use dotcore_proto::dotcore::site::store::v1::SiteCurrentState;
    use dotcore_site::store::{FileCurrentState, InMemoryCurrentState};

    use super::*;

    #[test]
    fn observed_placement_validation_enforces_the_shared_site_node_identifier_rule() {
        let accepted = deployment_with_placement("node-one", &["nvidia.com/gpu=0"]);

        assert!(validate_deployment(&accepted, true).is_ok());
        assert_eq!(
            accepted.state.expect("state").observed_placements,
            vec![ObservedPlacement {
                managed_node_id: SiteNodeId::for_test("node-one").to_string(),
                accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
            }]
        );

        for node_id in ["", "Node-One", "node_one"] {
            assert!(matches!(
                validate_deployment(
                    &deployment_with_placement(node_id, &["nvidia.com/gpu=0"]),
                    true
                ),
                Err(CurrentStateError::InvalidState)
            ));
        }
        assert!(matches!(
            validate_deployment(&deployment_with_placement("node-one", &[""]), true),
            Err(CurrentStateError::InvalidState)
        ));
    }

    #[test]
    fn persisted_state_rejects_an_invalid_observed_placement_identity() {
        let accepted = site_current_state(&deployment_with_placement(
            "node-one",
            &["nvidia.com/gpu=0"],
        ));
        assert!(validate_state(&accepted).is_ok());

        let rejected = site_current_state(&deployment_with_placement(
            "Node-One",
            &["nvidia.com/gpu=0"],
        ));
        assert!(matches!(
            validate_state(&rejected),
            Err(CurrentStateError::InvalidState)
        ));
    }

    #[test]
    fn progress_validation_accepts_exact_known_and_unknown_totals() {
        assert!(!invalid_progress(&progress(Some(25), 256, Some(1024))));
        assert!(!invalid_progress(&progress(None, 256, None)));
    }

    #[test]
    fn progress_validation_rejects_every_inconsistent_shape() {
        let mut unspecified = progress(Some(25), 256, Some(1024));
        unspecified.unit = ModelDeploymentProgressUnit::Unspecified as i32;

        assert!(invalid_progress(&unspecified));
        assert!(invalid_progress(&progress(Some(101), 101, Some(100))));
        assert!(invalid_progress(&progress(Some(0), 0, Some(0))));
        assert!(invalid_progress(&progress(Some(100), 101, Some(100))));
        assert!(invalid_progress(&progress(Some(25), 256, None)));
        assert!(invalid_progress(&progress(Some(24), 256, Some(1024))));
    }

    #[test]
    fn repository_revisions_isolate_snapshots_over_memory_and_file_stores() {
        let directory = crate::test_support::canonical_tempdir().expect("Site root");
        let site_yaml = directory.path().join("site.yaml");
        std::fs::write(&site_yaml, "siteId: site-one\n").expect("Site input");
        let file = FileCurrentState::open(&site_yaml, "site-one").expect("file state");

        assert_repository_revisions(&InMemoryCurrentState::new("site-one").expect("memory state"));
        assert_repository_revisions(&file);
        drop(file);

        let reopened = FileCurrentState::open(&site_yaml, "site-one").expect("reopened state");
        assert_eq!(
            reopened
                .snapshot()
                .expect("reopened state")
                .storage_revision,
            3
        );
    }

    #[test]
    fn oversized_and_noncanonical_records_leave_the_state_unchanged() {
        let state = InMemoryCurrentState::new("site-one").expect("memory state");
        state
            .upsert(loading("model-one"), None)
            .expect("model write");
        let before = state.snapshot().expect("snapshot");
        let mut oversized = loading("model-two");
        oversized.state.as_mut().expect("state").status_message = "x".repeat(4 * 1024 * 1024);

        assert!(matches!(
            state.upsert(oversized, None),
            Err(CurrentStateError::LimitExceeded)
        ));
        assert!(matches!(
            state.upsert(loading("deployment one"), None),
            Err(CurrentStateError::InvalidState)
        ));
        assert_eq!(state.snapshot().expect("unchanged state"), before);
    }

    fn assert_repository_revisions(repository: &impl CurrentStateRepository) {
        assert_eq!(
            repository
                .snapshot()
                .expect("initial state")
                .storage_revision,
            0
        );
        assert_eq!(
            repository
                .upsert(loading("deployment-one"), None)
                .expect("insert")
                .record_revision,
            1
        );
        let isolated = repository.snapshot().expect("isolated state");
        assert!(matches!(
            repository.upsert(loading("deployment-one"), None),
            Err(CurrentStateError::AlreadyExists)
        ));
        let mut update = loading("deployment-one");
        update.state.as_mut().expect("state").status_message = "Ready".to_owned();
        assert!(matches!(
            repository.upsert(update, Some(0)),
            Err(CurrentStateError::RevisionConflict)
        ));
        assert_eq!(
            repository
                .upsert(loading("deployment-one"), Some(1))
                .expect("update")
                .record_revision,
            2
        );
        assert_eq!(isolated.model_deployments[0].record_revision, 1);
        assert!(matches!(
            repository.remove("deployment-one", 1),
            Err(CurrentStateError::RevisionConflict)
        ));
        assert_eq!(
            repository
                .remove("deployment-one", 2)
                .expect("remove")
                .storage_revision,
            3
        );
        assert!(
            repository
                .snapshot()
                .expect("final state")
                .model_deployments
                .is_empty()
        );
        assert!(matches!(
            repository.upsert(CurrentModelDeployment::default(), None),
            Err(CurrentStateError::InvalidState)
        ));
    }

    fn loading(id: &str) -> CurrentModelDeployment {
        CurrentModelDeployment {
            model_deployment_id: id.to_owned(),
            state: Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Loading as i32,
                status_message: "Loading".to_owned(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn progress(
        percent: Option<u32>,
        completed: u64,
        total: Option<u64>,
    ) -> ModelDeploymentProgress {
        ModelDeploymentProgress {
            percent,
            completed,
            total,
            unit: ModelDeploymentProgressUnit::Bytes as i32,
        }
    }

    fn deployment_with_placement(
        managed_node_id: &str,
        accelerator_device_ids: &[&str],
    ) -> CurrentModelDeployment {
        CurrentModelDeployment {
            model_deployment_id: "deployment-a".to_owned(),
            reconciled_lifecycle_generation: None,
            record_revision: 1,
            recovery_barrier: None,
            state: Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Ready as i32,
                status_message: "Ready".to_owned(),
                observed_placements: vec![ObservedPlacement {
                    managed_node_id: managed_node_id.to_owned(),
                    accelerator_device_ids: accelerator_device_ids
                        .iter()
                        .map(|id| (*id).to_owned())
                        .collect(),
                }],
                ..Default::default()
            }),
        }
    }

    fn site_current_state(deployment: &CurrentModelDeployment) -> SiteCurrentState {
        SiteCurrentState {
            site_id: "site-one".to_owned(),
            storage_revision: 1,
            model_deployments: vec![deployment.clone()],
            ..Default::default()
        }
    }
}
