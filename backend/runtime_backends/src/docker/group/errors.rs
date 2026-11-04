//! Typed failures the Docker runtime retains when a workload group cannot be created or started
//! and its cleanup or rollback also fails.
//!
//! Each failure travels inside the generic Docker runtime error and names the exact owned
//! containers and network that controller discovery must remove before a retry.

use std::collections::BTreeMap;

use dotcore_runtime::{RuntimeBackendError, WorkloadGroupId, WorkloadUnitName};

/// Failures retained after Docker group creation and cleanup cannot complete.
///
/// `Error::source()` returns the creation failure because it caused cleanup to begin. Accessors
/// expose every retained failure, including an absence-inspection failure when present.
#[derive(Debug, thiserror::Error)]
pub enum DockerGroupCreationError {
    #[error("Docker group creation failed and owned-resource cleanup failed")]
    CreationAndCleanup {
        #[source]
        creation_error: RuntimeBackendError,

        cleanup_error: RuntimeBackendError,

        owned_realization: Option<OwnedWorkloadGroupRealization>,
    },

    #[error(
        "Docker group creation failed, owned-resource cleanup failed, and absence inspection failed"
    )]
    CreationCleanupAndInspection {
        #[source]
        creation_error: RuntimeBackendError,

        cleanup_error: RuntimeBackendError,

        absence_inspection_error: RuntimeBackendError,

        owned_realization: OwnedWorkloadGroupRealization,
    },
}

impl DockerGroupCreationError {
    /// Returns the retained creation failure carried by a Docker runtime error, when present.
    pub fn from_runtime_error(error: &RuntimeBackendError) -> Option<&Self> {
        let RuntimeBackendError::Docker(error) = error else {
            return None;
        };
        error.downcast_ref()
    }

    /// Returns the creation failure that caused cleanup to begin.
    pub fn creation_error(&self) -> &RuntimeBackendError {
        match self {
            Self::CreationAndCleanup { creation_error, .. }
            | Self::CreationCleanupAndInspection { creation_error, .. } => creation_error,
        }
    }

    /// Returns the cleanup failure that accompanied the creation failure.
    pub fn cleanup_error(&self) -> &RuntimeBackendError {
        match self {
            Self::CreationAndCleanup { cleanup_error, .. }
            | Self::CreationCleanupAndInspection { cleanup_error, .. } => cleanup_error,
        }
    }

    /// Returns the retained absence-inspection failure when cleanup completion was uncertain.
    pub fn absence_inspection_error(&self) -> Option<&RuntimeBackendError> {
        match self {
            Self::CreationAndCleanup { .. } => None,
            Self::CreationCleanupAndInspection {
                absence_inspection_error,
                ..
            } => Some(absence_inspection_error),
        }
    }

    /// Returns exact owned resources that controller discovery must remove before retry.
    pub fn owned_realization(&self) -> Option<&OwnedWorkloadGroupRealization> {
        match self {
            Self::CreationAndCleanup {
                owned_realization, ..
            } => owned_realization.as_ref(),
            Self::CreationCleanupAndInspection {
                owned_realization, ..
            } => Some(owned_realization),
        }
    }
}

impl From<DockerGroupCreationError> for RuntimeBackendError {
    fn from(error: DockerGroupCreationError) -> Self {
        Self::Docker(anyhow::Error::new(error))
    }
}

/// Failures retained after Docker group startup and rollback cannot complete.
#[derive(Debug, thiserror::Error)]
#[error("Docker group startup failed and started-unit rollback failed")]
pub struct DockerGroupStartError {
    #[source]
    start_error: Box<RuntimeBackendError>,

    rollback_error: Box<RuntimeBackendError>,

    owned_realization: OwnedWorkloadGroupRealization,
}

impl DockerGroupStartError {
    pub(super) fn new(
        start_error: RuntimeBackendError,
        rollback_error: RuntimeBackendError,
        owned_realization: OwnedWorkloadGroupRealization,
    ) -> Self {
        Self {
            start_error: Box::new(start_error),
            rollback_error: Box::new(rollback_error),
            owned_realization,
        }
    }

    /// Returns the retained startup failure carried by a Docker runtime error, when present.
    pub fn from_runtime_error(error: &RuntimeBackendError) -> Option<&Self> {
        let RuntimeBackendError::Docker(error) = error else {
            return None;
        };
        error.downcast_ref()
    }

    /// Returns the startup failure that caused rollback to begin.
    pub fn start_error(&self) -> &RuntimeBackendError {
        &self.start_error
    }

    /// Returns the failure encountered while stopping units started by the attempt.
    pub fn rollback_error(&self) -> &RuntimeBackendError {
        &self.rollback_error
    }

    /// Returns exact owned resources that controller discovery must remove before retry.
    pub fn owned_realization(&self) -> &OwnedWorkloadGroupRealization {
        &self.owned_realization
    }
}

impl From<DockerGroupStartError> for RuntimeBackendError {
    fn from(error: DockerGroupStartError) -> Self {
        Self::Docker(anyhow::Error::new(error))
    }
}

/// Exact Docker-owned resources left after an incomplete workload-group realization cleanup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnedWorkloadGroupRealization {
    pub group_id: WorkloadGroupId,
    pub network_id: String,
    pub units: BTreeMap<WorkloadUnitName, String>,
}
