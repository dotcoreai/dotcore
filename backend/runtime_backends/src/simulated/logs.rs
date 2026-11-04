//! Deterministic diagnostic lines served by the in-memory runtime for every workload and workload
//! group unit it retains.
//!
//! Each retained workload, and each unit of a retained group, owns the same fixed history of
//! numbered stdout lines, two seconds apart from a constant epoch, for example "simulated
//! workload log line 003 owner=deployment-01 generation=1", with " unit=web" appended for a group
//! unit. Nothing depends on the wall clock or on the lifecycle state, so paging and follow
//! behavior can be asserted exactly.

use std::time::{Duration, UNIX_EPOCH};

use dotcore_runtime::{
    RuntimeBackendError, RuntimeGroupLogFollowQuery, RuntimeLogEntry, RuntimeLogEvent,
    RuntimeLogHistory, RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceHistory,
    RuntimeLogSourceSelection, RuntimeLogSourceStream, RuntimeLogStream, RuntimeLogStreamKind,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadCorrelation,
    WorkloadGroupLaunch, WorkloadUnitName,
};
use futures_util::{StreamExt as _, stream};
use tokio_util::sync::CancellationToken;

use super::adapter::InMemoryRuntimeBackend;

const LOG_LINE_COUNT: u64 = 12;
const LOG_EPOCH_SECONDS: u64 = 1_700_000_000;
// Two seconds apart so that paging by the whole second before the oldest line, an exclusive
// boundary, never skips a line.
const LOG_SPACING_SECONDS: u64 = 2;

impl InMemoryRuntimeBackend {
    /// Reads the newest lines strictly before the optional timestamp for the exactly matching workload.
    pub(super) fn read_logs_by_correlation(
        &self,
        correlation: &WorkloadCorrelation,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeBackendError> {
        self.require_workload(correlation)?;
        Ok(history_page(
            log_entries(query.source(), correlation, None),
            query.history(),
        ))
    }

    /// Replays the lines after the optional resume position, then stays open until cancelled.
    pub(super) fn follow_logs_by_correlation(
        &self,
        correlation: &WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> Result<RuntimeLogStream, RuntimeBackendError> {
        let cancellation = query.follow().cancellation().clone();
        if cancellation.is_cancelled() {
            return Err(RuntimeBackendError::LogCancelled);
        }
        self.require_workload(correlation)?;
        Ok(replay_then_hold(
            log_entries(query.source(), correlation, None),
            query.follow().since_timestamp(),
            cancellation,
        ))
    }

    /// Reads the newest lines of each selected unit of the exactly retained group; every source
    /// of a group that is not retained reads as not created.
    pub(super) fn read_group_logs(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: &RuntimeLogQuery,
    ) -> Result<Vec<RuntimeLogSourceHistory>, RuntimeBackendError> {
        let retained = self.require_group_units(group, sources)?;
        Ok(sources
            .sources()
            .iter()
            .map(|source| RuntimeLogSourceHistory {
                source: source.clone(),
                result: retained
                    .then(|| {
                        history_page(
                            log_entries(source, &group.correlation, Some(&source.name)),
                            query,
                        )
                    })
                    .ok_or_else(RuntimeBackendError::workload_not_created),
            })
            .collect())
    }

    /// Replays each selected unit's lines after its resume position, then holds every source
    /// open until cancelled; every source of a group that is not retained reads as not created.
    pub(super) fn follow_group_logs(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> Result<Vec<RuntimeLogSourceStream>, RuntimeBackendError> {
        if query.cancellation().is_cancelled() {
            return Err(RuntimeBackendError::LogCancelled);
        }
        let retained = self.require_group_units(group, sources)?;
        Ok(sources
            .sources()
            .iter()
            .map(|source| RuntimeLogSourceStream {
                source: source.clone(),
                result: retained
                    .then(|| {
                        replay_then_hold(
                            log_entries(source, &group.correlation, Some(&source.name)),
                            query.resume_timestamp(&source.name),
                            query.cancellation().clone(),
                        )
                    })
                    .ok_or_else(RuntimeBackendError::workload_not_created),
            })
            .collect())
    }

    /// Checks that every selected source names a unit of `group`, then whether it is retained.
    fn require_group_units(
        &self,
        group: &WorkloadGroupLaunch,
        sources: &RuntimeLogSourceSelection,
    ) -> Result<bool, RuntimeBackendError> {
        if sources.sources().iter().any(|source| {
            !group
                .units
                .contains_key(&WorkloadUnitName(source.name.clone()))
        }) {
            return Err(RuntimeBackendError::invalid_launch(
                "simulated workload group log source is not a declared unit",
            ));
        }
        self.retains_group(group)
    }

    fn require_workload(
        &self,
        correlation: &WorkloadCorrelation,
    ) -> Result<(), RuntimeBackendError> {
        self.state
            .lock()
            .expect("simulated runtime state")
            .workloads
            .values()
            .any(|workload| workload.identity.correlation == *correlation)
            .then_some(())
            .ok_or_else(RuntimeBackendError::workload_not_created)
    }
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

/// Streams the entries after the optional position, then stays open until cancelled.
fn replay_then_hold(
    mut entries: Vec<RuntimeLogEntry>,
    since: Option<std::time::SystemTime>,
    cancellation: CancellationToken,
) -> RuntimeLogStream {
    if let Some(since) = since {
        entries.retain(|entry| entry.timestamp > since);
    }
    let replay = stream::iter(
        entries
            .into_iter()
            .map(|entry| Ok(RuntimeLogEvent::Entry(entry))),
    );
    Box::pin(
        replay
            .chain(stream::pending())
            .take_until(cancellation.cancelled_owned()),
    )
}

fn log_entries(
    source: &RuntimeLogSource,
    correlation: &WorkloadCorrelation,
    unit: Option<&str>,
) -> Vec<RuntimeLogEntry> {
    (1..=LOG_LINE_COUNT)
        .map(|line| RuntimeLogEntry {
            source: source.clone(),
            timestamp: UNIX_EPOCH
                + Duration::from_secs(LOG_EPOCH_SECONDS + line * LOG_SPACING_SECONDS),
            stream: RuntimeLogStreamKind::Stdout,
            content: match unit {
                None => format!(
                    "simulated workload log line {line:03} owner={} generation={}",
                    correlation.owner_id, correlation.reconcile_generation
                ),
                Some(unit) => format!(
                    "simulated workload log line {line:03} owner={} generation={} unit={unit}",
                    correlation.owner_id, correlation.reconcile_generation
                ),
            }
            .into_bytes(),
            truncated: false,
        })
        .collect()
}
