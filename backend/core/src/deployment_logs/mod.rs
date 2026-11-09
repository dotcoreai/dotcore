//! Reads runtime logs of node-hosted deployments, on demand, for the site manager's
//! deployment-log management services, whatever controller owns the deployment and whichever
//! node hosts it.
//!
//! The node side answers manager requests from its own local runtime, fenced by manager session
//! and node incarnation, admitted and bounded before anything is read; a node in another process
//! serves them on its private listener. The manager side calls the hosting node, over the shared
//! pinned channel or in process for the node sharing its process, and turns its answers back into
//! the per-source runtime results the management pipeline already consumes. Workload output is
//! never retained; the hosting node retains only the bounded output of each deployment's
//! preparation steps until its realization is removed. Log data passes through neither node agent
//! nor observability.

mod client;
mod lifecycle;
mod preparation;
mod service;
#[cfg(test)]
mod tests;

use std::{
    future::Future,
    pin::Pin,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_proto::{
    dotcore::deployment::private::v1::{
        DeploymentLogEntry, DeploymentLogStream, DeploymentLogUnavailableReason,
    },
    google::protobuf::Timestamp,
};
use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogEntry, RuntimeLogSource, RuntimeLogSourceHistory,
    RuntimeLogSourceStream, RuntimeLogStreamKind,
};
use tokio::sync::mpsc;

use crate::management_logs::{
    DeploymentLogError, DeploymentLogFollowQuery, DeploymentLogReadQuery, MAX_LOG_RESPONSE_BYTES,
    ProtectedLogValues, filter_entry,
};
pub(crate) use client::{NodeDeploymentLogs, NodeLogRequest, NodeLogSelection};
pub(crate) use lifecycle::{WorkloadChange, WorkloadChanges, WorkloadLifecycle};
pub(crate) use preparation::{
    PreparationLogs, PreparationOwner, SourceSelection, model_steps, service_steps,
    step_from_source_name, step_source,
};
pub(crate) use service::{DeploymentLogOwners, DeploymentLogReader, DeploymentLogService};

/// Largest encoded log history a node returns, the same budget as a management page.
const RESPONSE_BUDGET_BYTES: usize = MAX_LOG_RESPONSE_BYTES;

/// Largest encoded private log message in either direction: a full history budget plus room for
/// its framing.
const MESSAGE_LIMIT_BYTES: usize = RESPONSE_BUDGET_BYTES + 64 * 1024;

/// Longest a manager waits for a node's history, or for the opening frame of a follow, so a
/// hung node cannot hold a management request.
/// Not a behavior duration: bound on a real call to a node's log service.
const WORKER_LOG_DEADLINE: Duration = Duration::from_secs(10);

/// Boxed future returned by one deployment owner's log operation.
pub(crate) type DeploymentLogOwnerFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, DeploymentLogError>> + Send + 'a>>;

/// Node-local log access for the deployments of one owner kind, such as model deployments.
///
/// An owner answers only for deployments assigned in the node's installed view, resolves their
/// workload itself, and reads only its exact local runtime.
pub(crate) trait DeploymentLogOwner: Send + Sync {
    /// Reads one bounded history per selected source of an installed deployment.
    fn read_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogReadQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceHistory>>;

    /// Opens one follow per selected source of an installed deployment; each stops when the
    /// query's cancellation fires. A follow selecting every source also lists the sources that
    /// start while it runs, and the changes of the deployment's workload from before it read
    /// its current state.
    fn follow_logs<'a>(
        &'a self,
        deployment_id: &'a str,
        query: DeploymentLogFollowQuery,
    ) -> DeploymentLogOwnerFuture<'a, OwnerLogSources<RuntimeLogSourceStream>>;
}

/// One owner's per-source results for an installed deployment, with the values resolved for its
/// launch, such as application environment values, that must never leave the node.
pub(crate) struct OwnerLogSources<T> {
    /// Per-source results, in selection order.
    pub(crate) sources: Vec<T>,

    /// Values the node redacts from every entry and diagnostic before anything is cut or sent.
    pub(crate) protected: ProtectedLogValues,

    /// Follows of the sources that start after the follow opened, such as a preparation step;
    /// absent for histories and for follows naming their sources.
    pub(crate) late: Option<mpsc::Receiver<RuntimeLogSourceStream>>,

    /// Changes of the deployment's workload since just before the owner read its current state,
    /// each of which the follow announces again; absent for histories and for follows naming
    /// their sources.
    pub(crate) changes: Option<WorkloadChanges>,
}

impl<T> OwnerLogSources<T> {
    /// Results of a deployment whose launch holds no protected values.
    pub(crate) fn unprotected(sources: Vec<T>) -> Self {
        Self::protected(sources, ProtectedLogValues::default())
    }

    /// Results whose entries and diagnostics the node redacts `protected` from.
    pub(crate) fn protected(sources: Vec<T>, protected: ProtectedLogValues) -> Self {
        Self {
            sources,
            protected,
            late: None,
            changes: None,
        }
    }

    /// Adds the follows of sources that start later.
    pub(crate) fn with_late(
        mut self,
        late: Option<mpsc::Receiver<RuntimeLogSourceStream>>,
    ) -> Self {
        self.late = late;
        self
    }

    /// Adds the changes of the deployment's workload the follow announces.
    pub(crate) fn with_changes(mut self, changes: Option<WorkloadChanges>) -> Self {
        self.changes = changes;
        self
    }
}

/// Converts one runtime entry to its private form: protected values are redacted first, then the
/// content is cut to the management entry limit, so a value spanning the cut cannot leak a part.
fn private_entry(entry: RuntimeLogEntry, protected: &ProtectedLogValues) -> DeploymentLogEntry {
    let entry = filter_entry(entry, protected);
    DeploymentLogEntry {
        timestamp: Some(timestamp(entry.timestamp)),
        stream: match entry.stream {
            RuntimeLogStreamKind::Stdout => DeploymentLogStream::Stdout,
            RuntimeLogStreamKind::Stderr => DeploymentLogStream::Stderr,
        } as i32,
        content: entry.content,
        truncated: entry.truncated,
    }
}

/// Converts one private entry back to a runtime entry of `source`; `None` when it is malformed.
fn runtime_entry(entry: DeploymentLogEntry, source: &RuntimeLogSource) -> Option<RuntimeLogEntry> {
    let stream = match DeploymentLogStream::try_from(entry.stream).ok()? {
        DeploymentLogStream::Stdout => RuntimeLogStreamKind::Stdout,
        DeploymentLogStream::Stderr => RuntimeLogStreamKind::Stderr,
        DeploymentLogStream::Unspecified => return None,
    };
    Some(RuntimeLogEntry {
        source: source.clone(),
        timestamp: system_time(entry.timestamp?)?,
        stream,
        content: entry.content,
        truncated: entry.truncated,
    })
}

/// Names a runtime log failure by the typed code the private contract carries.
fn unavailable_reason(error: &RuntimeBackendError) -> DeploymentLogUnavailableReason {
    match error {
        RuntimeBackendError::LogCapabilityUnavailable => {
            DeploymentLogUnavailableReason::LogCapabilityUnavailable
        }
        RuntimeBackendError::WorkloadNotCreated => {
            DeploymentLogUnavailableReason::WorkloadNotCreated
        }
        RuntimeBackendError::LogSourceLimitReached => {
            DeploymentLogUnavailableReason::SourceLimitReached
        }
        _ => DeploymentLogUnavailableReason::RuntimeFailed,
    }
}

/// Rebuilds the runtime failure a typed code stands for. Any other failure stays on the node,
/// so it becomes the expected remote-unavailable outcome.
fn runtime_error(reason: i32) -> RuntimeBackendError {
    match DeploymentLogUnavailableReason::try_from(reason) {
        Ok(DeploymentLogUnavailableReason::LogCapabilityUnavailable) => {
            RuntimeBackendError::log_capability_unavailable()
        }
        Ok(DeploymentLogUnavailableReason::WorkloadNotCreated) => {
            RuntimeBackendError::workload_not_created()
        }
        Ok(DeploymentLogUnavailableReason::SourceLimitReached) => {
            RuntimeBackendError::LogSourceLimitReached
        }
        _ => RuntimeBackendError::remote_log_unavailable(),
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

fn system_time(timestamp: Timestamp) -> Option<SystemTime> {
    let seconds = u64::try_from(timestamp.seconds).ok()?;
    let nanos = u32::try_from(timestamp.nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    Some(UNIX_EPOCH + Duration::new(seconds, nanos))
}
