//! Serves a native workload's retained output lines as direct runtime diagnostics.
//!
//! Records come from the launcher's per-stream log files; each line keeps the time the launcher
//! read it and the stream it came from. History pages and follow streams follow the same
//! `before` and `since` semantics as the other runtimes.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dotcore_launcher::{
    supervisor::{LogFollower, SupervisorError},
    workload::{LogRecord, LogStream},
};
use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent, RuntimeLogHistory, RuntimeLogQuery,
    RuntimeLogSource, RuntimeLogStream, RuntimeLogStreamKind, RuntimeWorkloadLogFollowQuery,
    RuntimeWorkloadLogQuery, WorkloadCorrelation, WorkloadId,
};
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::adapter::{NativeRuntimeBackend, supervisor_error};

impl NativeRuntimeBackend {
    /// Reads the newest lines strictly before the optional timestamp of an owned workload.
    pub(super) async fn read_logs(
        &self,
        id: &str,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeBackendError> {
        self.require_live_workload(id).await?;
        let records =
            self.supervisor.logs(id).await.map_err(|error| {
                supervisor_error(format!("read native workload {id} logs"), error)
            })?;
        let entries = records
            .into_iter()
            .map(|record| log_entry(query.source(), record))
            .collect();
        Ok(history_page(entries, query.history()))
    }

    /// Replays the lines after the optional resume timestamp, then follows new ones until the
    /// workload's launcher exits or the workload is removed, which ends the stream. A relaunch
    /// starts the workload's output afresh, so following it again replays from its first line.
    pub(super) async fn follow_logs(
        &self,
        id: &str,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> Result<RuntimeLogStream, RuntimeBackendError> {
        let cancellation = query.follow().cancellation().clone();
        if cancellation.is_cancelled() {
            return Err(RuntimeBackendError::LogCancelled);
        }
        self.require_live_workload(id).await?;
        let follower = self.supervisor.follow_logs(id).map_err(|error| {
            supervisor_error(format!("follow native workload {id} logs"), error)
        })?;
        let state = FollowState {
            follower,
            source: query.source().clone(),
            since: query.follow().since_timestamp(),
            directory: self.workload_directory(id),
            done: false,
        };
        let events = futures_util::stream::unfold(state, FollowState::next);
        Ok(Box::pin(events.take_until(cancelled(cancellation))))
    }

    pub(super) async fn read_logs_by_correlation(
        &self,
        correlation: &WorkloadCorrelation,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeBackendError> {
        let id = self.workload_id_for_correlation(correlation)?;
        self.read_logs(&id.0, query).await
    }

    pub(super) async fn follow_logs_by_correlation(
        &self,
        correlation: &WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> Result<RuntimeLogStream, RuntimeBackendError> {
        let id = self.workload_id_for_correlation(correlation)?;
        self.follow_logs(&id.0, query).await
    }

    /// Resolves the one known workload with exactly this correlation.
    fn workload_id_for_correlation(
        &self,
        correlation: &WorkloadCorrelation,
    ) -> Result<WorkloadId, RuntimeBackendError> {
        let matches = self.known_workload_ids(correlation);
        match matches.as_slice() {
            [workload_id] => Ok(workload_id.clone()),
            [] => Err(RuntimeBackendError::workload_not_created()),
            _ => Err(RuntimeBackendError::invalid_launch(
                "native runtime has ambiguous correlated workloads",
            )),
        }
    }
}

/// Follow position over one workload's log records.
struct FollowState {
    follower: LogFollower,
    source: RuntimeLogSource,
    since: Option<SystemTime>,
    directory: std::path::PathBuf,
    /// Whether the follower's failure was already produced.
    done: bool,
}

impl FollowState {
    /// Produces the next event: an entry after the resume position, nothing once the workload
    /// exited or is gone, so the source reads as ended, or the follower's failure.
    async fn next(mut self) -> Option<(Result<RuntimeLogEvent, RuntimeBackendError>, Self)> {
        if self.done {
            return None;
        }
        loop {
            match self.follower.next().await {
                Ok(Some(record)) => {
                    let entry = log_entry(&self.source, record);
                    if self.since.is_some_and(|since| entry.timestamp <= since) {
                        continue;
                    }
                    return Some((Ok(RuntimeLogEvent::Entry(entry)), self));
                }
                Ok(None) => return None,
                // A workload removed while followed has no directory left to read.
                Err(_) if !self.directory.exists() => return None,
                Err(error) => return Some(self.fail(error)),
            }
        }
    }

    fn fail(
        mut self,
        error: SupervisorError,
    ) -> (Result<RuntimeLogEvent, RuntimeBackendError>, Self) {
        self.done = true;
        (
            Err(supervisor_error("follow native workload logs", error)),
            self,
        )
    }
}

async fn cancelled(cancellation: CancellationToken) {
    cancellation.cancelled().await;
}

/// Returns the newest entries strictly before the query's optional timestamp.
fn history_page(mut entries: Vec<RuntimeLogEntry>, query: &RuntimeLogQuery) -> RuntimeLogHistory {
    if let Some(before) = query.before_timestamp() {
        entries.retain(|entry| entry.timestamp < before);
    }
    let maximum = usize::from(query.maximum_entries().get());
    let has_older_output = entries.len() > maximum;
    entries.drain(..entries.len().saturating_sub(maximum));
    RuntimeLogHistory {
        entries,
        has_older_output,
        candidate_budget_truncated: false,
    }
}

// Lines are already split at the launcher's line limit, so no entry is truncated here.
fn log_entry(source: &RuntimeLogSource, record: LogRecord) -> RuntimeLogEntry {
    RuntimeLogEntry {
        source: source.clone(),
        timestamp: UNIX_EPOCH + Duration::from_nanos(record.time_ns),
        stream: match record.stream {
            LogStream::Stdout => RuntimeLogStreamKind::Stdout,
            LogStream::Stderr => RuntimeLogStreamKind::Stderr,
        },
        content: record.line.into_bytes(),
        truncated: false,
    }
}
