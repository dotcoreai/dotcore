//! Docker-owned workload diagnostic source acquisition and record normalization.

use std::{collections::VecDeque, time::SystemTime};

use dotcore_domain::docker::DOCKER_OPERATION_TIMEOUT;
use dotcore_domain::real_time;
use dotcore_runtime::{
    MAX_RUNTIME_LOG_ENTRIES, RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent,
    RuntimeLogHistory, RuntimeLogSource, RuntimeLogStream, RuntimeLogStreamKind,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadId,
};
use futures_util::{StreamExt as _, stream};

use super::{
    adapter::DockerAdapter,
    client::{DockerClient, DockerLogOutput, DockerLogRequest, DockerLogStreamKind},
};

pub(super) const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
pub(super) const HISTORY_CANDIDATE_BYTES: usize = 4 * 1024 * 1024;
const LOG_ENTRY_OVERHEAD_BYTES: usize = 256;

impl<C: DockerClient> DockerAdapter<C> {
    pub(super) async fn read_logs(
        &self,
        workload_id: &WorkloadId,
        query: &RuntimeWorkloadLogQuery,
    ) -> Result<RuntimeLogHistory, RuntimeBackendError> {
        real_time::bound(DOCKER_OPERATION_TIMEOUT, async {
            let known = self.live_known_workload(workload_id).await?;
            self.read_exact_logs(
                known.container_id,
                query.source(),
                query.history(),
                HISTORY_CANDIDATE_BYTES,
            )
            .await
        })
        .await
        .map_err(history_timeout)?
    }

    pub(super) async fn follow_logs(
        &self,
        workload_id: &WorkloadId,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> Result<RuntimeLogStream, RuntimeBackendError> {
        let cancellation = query.follow().cancellation().clone();
        let known = tokio::select! {
            _ = cancellation.cancelled() => return Err(RuntimeBackendError::LogCancelled),
            known = self.live_known_workload(workload_id) => known?,
        };
        self.follow_exact_logs(
            known.container_id,
            query.source().clone(),
            query.follow().since_timestamp(),
            cancellation,
        )
    }

    pub(super) async fn read_exact_logs(
        &self,
        container_id: String,
        source: &RuntimeLogSource,
        query: &dotcore_runtime::RuntimeLogQuery,
        candidate_bytes: usize,
    ) -> Result<RuntimeLogHistory, RuntimeBackendError> {
        let _permit = self.config.source_stream_permits.try_acquire()?;
        let tail = query.maximum_entries().get().saturating_add(1);
        let mut logs = self.client.container_logs(DockerLogRequest {
            container_id,
            follow: false,
            since: None,
            until: query.before_timestamp(),
            tail: Some(tail),
        });
        let mut normalizer = LogNormalizer::new(source.clone());
        let mut entries = VecDeque::with_capacity(usize::from(tail));
        let mut retained_bytes = 0;
        let mut has_older_output = false;
        let mut candidate_budget_truncated = false;
        while let Some(output) = logs.next().await {
            let output = output.map_err(log_client_error)?;
            normalizer.for_each_entry(output, |entry| {
                let retention = retain_candidate(
                    &mut entries,
                    &mut retained_bytes,
                    entry,
                    usize::from(tail),
                    candidate_bytes,
                );
                has_older_output |= retention.discarded;
                candidate_budget_truncated |= retention.byte_budget;
            });
        }
        while let Some(entry) = normalizer.finish() {
            let retention = retain_candidate(
                &mut entries,
                &mut retained_bytes,
                entry,
                usize::from(tail),
                candidate_bytes,
            );
            has_older_output |= retention.discarded;
            candidate_budget_truncated |= retention.byte_budget;
        }
        while entries.len() > usize::from(query.maximum_entries().get()) {
            entries.pop_front();
            has_older_output = true;
        }
        Ok(RuntimeLogHistory {
            entries: entries.into(),
            has_older_output,
            candidate_budget_truncated,
        })
    }

    pub(super) fn follow_exact_logs(
        &self,
        container_id: String,
        source: RuntimeLogSource,
        resume_timestamp: Option<SystemTime>,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<RuntimeLogStream, RuntimeBackendError> {
        if cancellation.is_cancelled() {
            return Err(RuntimeBackendError::LogCancelled);
        }
        let permit = self.config.source_stream_permits.try_acquire()?;
        let logs = self.client.container_logs(DockerLogRequest {
            container_id,
            follow: true,
            since: resume_timestamp.map(whole_seconds),
            until: None,
            // Without a resume position a long-running container would replay its whole output.
            tail: resume_timestamp
                .is_none()
                .then_some(MAX_RUNTIME_LOG_ENTRIES),
        });
        let state = FollowState {
            logs,
            _permit: permit,
            normalizer: LogNormalizer::new(source),
            resume_timestamp,
            output: None,
            cancellation,
            logs_ended: false,
            finished: false,
        };
        Ok(Box::pin(stream::unfold(state, |mut state| async move {
            loop {
                if let Some(output) = state.output.as_mut() {
                    if let Some(entry) = state.normalizer.next_entry(output) {
                        return Some((Ok(RuntimeLogEvent::Entry(entry)), state));
                    }
                    state.output = None;
                }
                if state.finished {
                    return None;
                }
                if state.logs_ended {
                    // Both streams may hold a trailing line without a newline.
                    match state.normalizer.finish() {
                        Some(entry) => return Some((Ok(RuntimeLogEvent::Entry(entry)), state)),
                        None => {
                            state.finished = true;
                            continue;
                        }
                    }
                }
                tokio::select! {
                    _ = state.cancellation.cancelled() => return None,
                    output = state.logs.next() => match output {
                        Some(Ok(output)) => {
                            if !state.is_replayed(&output) {
                                state.output = Some(PendingDockerLogOutput::new(output));
                            }
                        }
                        Some(Err(error)) => return Some((Err(log_client_error(error)), state)),
                        None => state.logs_ended = true,
                    },
                }
            }
        })))
    }
}

struct FollowState {
    logs: super::client::DockerLogStream,
    _permit: tokio::sync::OwnedSemaphorePermit,
    normalizer: LogNormalizer,
    resume_timestamp: Option<SystemTime>,
    output: Option<PendingDockerLogOutput>,
    cancellation: tokio_util::sync::CancellationToken,
    /// Whether Docker's stream ended, leaving only partial lines to flush.
    logs_ended: bool,
    finished: bool,
}

/// Floors a timestamp to whole seconds, the granularity Docker's `since` filter works at.
fn whole_seconds(timestamp: SystemTime) -> SystemTime {
    let seconds = timestamp
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(seconds)
}

impl FollowState {
    /// Whether the record is at or before the resume position, so the caller already has it.
    /// Records are filtered before line assembly so a replayed partial line cannot merge with a
    /// later record and take its timestamp.
    fn is_replayed(&self, output: &DockerLogOutput) -> bool {
        self.resume_timestamp
            .is_some_and(|resume| output.timestamp <= resume)
    }
}

struct PendingDockerLogOutput {
    output: DockerLogOutput,
    offset: usize,
}

impl PendingDockerLogOutput {
    fn new(output: DockerLogOutput) -> Self {
        Self { output, offset: 0 }
    }
}

struct LogNormalizer {
    source: RuntimeLogSource,
    stdout: LineState,
    stderr: LineState,
}

impl LogNormalizer {
    fn new(source: RuntimeLogSource) -> Self {
        Self {
            source,
            stdout: LineState::default(),
            stderr: LineState::default(),
        }
    }

    fn for_each_entry(
        &mut self,
        output: DockerLogOutput,
        mut consume: impl FnMut(RuntimeLogEntry),
    ) {
        let mut pending = PendingDockerLogOutput::new(output);
        while let Some(entry) = self.next_entry(&mut pending) {
            consume(entry);
        }
    }

    fn next_entry(&mut self, pending: &mut PendingDockerLogOutput) -> Option<RuntimeLogEntry> {
        let (state, stream) = match pending.output.stream {
            DockerLogStreamKind::Stdout => (&mut self.stdout, RuntimeLogStreamKind::Stdout),
            DockerLogStreamKind::Stderr => (&mut self.stderr, RuntimeLogStreamKind::Stderr),
        };
        let remaining = &pending.output.content[pending.offset..];
        let (entry, consumed) = state.push_until_entry(
            &self.source,
            stream,
            pending.output.timestamp,
            remaining,
            pending.output.truncated,
        );
        pending.offset += consumed;
        entry
    }

    fn finish(&mut self) -> Option<RuntimeLogEntry> {
        self.stdout
            .finish(&self.source, RuntimeLogStreamKind::Stdout)
            .or_else(|| {
                self.stderr
                    .finish(&self.source, RuntimeLogStreamKind::Stderr)
            })
    }
}

#[derive(Default)]
struct LineState {
    timestamp: Option<SystemTime>,
    content: Vec<u8>,
    truncated: bool,
}

impl LineState {
    fn push_until_entry(
        &mut self,
        source: &RuntimeLogSource,
        stream: RuntimeLogStreamKind,
        timestamp: SystemTime,
        bytes: &[u8],
        input_truncated: bool,
    ) -> (Option<RuntimeLogEntry>, usize) {
        for (offset, byte) in bytes.iter().enumerate() {
            self.timestamp.get_or_insert(timestamp);
            if *byte == b'\n' {
                if input_truncated && offset + 1 == bytes.len() {
                    self.truncated = true;
                }
                return (Some(self.take(source, stream)), offset + 1);
            }
            if self.content.len() < MAX_LOG_LINE_BYTES {
                self.content.push(*byte);
            } else {
                self.truncated = true;
            }
        }
        self.truncated |= input_truncated;
        (None, bytes.len())
    }

    fn finish(
        &mut self,
        source: &RuntimeLogSource,
        stream: RuntimeLogStreamKind,
    ) -> Option<RuntimeLogEntry> {
        self.timestamp.map(|_| self.take(source, stream))
    }

    fn take(&mut self, source: &RuntimeLogSource, stream: RuntimeLogStreamKind) -> RuntimeLogEntry {
        while self.content.last() == Some(&b'\r') {
            self.content.pop();
        }
        RuntimeLogEntry {
            source: source.clone(),
            timestamp: self.timestamp.take().expect("started log line"),
            stream,
            content: std::mem::take(&mut self.content),
            truncated: std::mem::take(&mut self.truncated),
        }
    }
}

fn retain_candidate(
    entries: &mut VecDeque<RuntimeLogEntry>,
    retained_bytes: &mut usize,
    entry: RuntimeLogEntry,
    maximum_entries: usize,
    maximum_bytes: usize,
) -> CandidateRetention {
    *retained_bytes += candidate_size(&entry);
    entries.push_back(entry);
    let mut retention = CandidateRetention::default();
    while entries.len() > maximum_entries || *retained_bytes > maximum_bytes {
        retention.discarded = true;
        retention.byte_budget |= *retained_bytes > maximum_bytes;
        let removed = entries.pop_front().expect("candidate exceeding a bound");
        *retained_bytes = retained_bytes.saturating_sub(candidate_size(&removed));
    }
    retention
}

#[derive(Default)]
struct CandidateRetention {
    discarded: bool,
    byte_budget: bool,
}

fn candidate_size(entry: &RuntimeLogEntry) -> usize {
    entry.content.len()
        + entry.source.name.len()
        + entry.source.label.len()
        + LOG_ENTRY_OVERHEAD_BYTES
}

fn history_timeout(source: tokio::time::error::Elapsed) -> RuntimeBackendError {
    super::runtime_error_with_source(
        "Docker log history acquisition timed out",
        super::client::ClientError::Timeout {
            operation: "Docker log history acquisition",
            source,
        },
    )
}

fn log_client_error(error: super::client::ClientError) -> RuntimeBackendError {
    super::runtime_error_with_source("Docker log acquisition failed", error)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn candidate_retention_reports_entry_and_byte_budget_evidence() {
        let source = RuntimeLogSource {
            name: "model".to_owned(),
            label: "Model".to_owned(),
        };
        let entry = |content: &[u8]| RuntimeLogEntry {
            source: source.clone(),
            timestamp: UNIX_EPOCH,
            stream: RuntimeLogStreamKind::Stdout,
            content: content.to_vec(),
            truncated: false,
        };
        let mut entries = VecDeque::new();
        let mut bytes = 0;

        assert!(
            !retain_candidate(&mut entries, &mut bytes, entry(b"one"), 2, usize::MAX).discarded
        );
        let count = retain_candidate(&mut entries, &mut bytes, entry(b"two"), 1, usize::MAX);
        assert!(count.discarded);
        assert!(!count.byte_budget);
        let bytes_limit = retain_candidate(&mut entries, &mut bytes, entry(b"three"), 2, 1);
        assert!(bytes_limit.discarded);
        assert!(bytes_limit.byte_budget);
        assert!(entries.is_empty());
    }

    #[test]
    fn normalizer_frames_multiline_chunks_and_discards_oversized_remainders() {
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        let mut normalizer = LogNormalizer::new(source.clone());
        let mut first = vec![b'x'; MAX_LOG_LINE_BYTES + 9];
        first.extend_from_slice(b"\nsecond\npar");

        let mut entries = Vec::new();
        normalizer.for_each_entry(DockerLogOutput::stdout(timestamp, first), |entry| {
            entries.push(entry);
        });
        normalizer.for_each_entry(
            DockerLogOutput::stdout(timestamp + Duration::from_nanos(1), b"tial\r\n".to_vec()),
            |entry| entries.push(entry),
        );

        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].content, vec![b'x'; MAX_LOG_LINE_BYTES]);
        assert!(entries[0].truncated);
        assert_eq!(entries[1].content, b"second");
        assert_eq!(entries[2].content, b"partial");
        assert_eq!(entries[2].timestamp, timestamp);
        assert!(entries.iter().all(|entry| entry.source == source));
    }

    #[test]
    fn normalizer_preserves_framer_truncation() {
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };
        let output = DockerLogOutput {
            stream: DockerLogStreamKind::Stdout,
            timestamp: UNIX_EPOCH,
            content: b"bounded\n".to_vec(),
            truncated: true,
        };
        let mut normalizer = LogNormalizer::new(source);
        let mut entries = Vec::new();

        normalizer.for_each_entry(output, |entry| entries.push(entry));

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, b"bounded");
        assert!(entries[0].truncated);
    }

    #[test]
    fn normalizer_incrementally_frames_a_newline_dense_chunk() {
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };
        let timestamp = UNIX_EPOCH + Duration::from_secs(1_704_067_200);
        let mut normalizer = LogNormalizer::new(source.clone());
        let mut output =
            PendingDockerLogOutput::new(DockerLogOutput::stdout(timestamp, vec![b'\n'; 10_000]));

        for _ in 0..10_000 {
            let entry = normalizer
                .next_entry(&mut output)
                .expect("one record per normalization step");
            assert_eq!(entry.content, b"");
            assert_eq!(entry.timestamp, timestamp);
            assert_eq!(entry.source, source);
        }
        assert!(normalizer.next_entry(&mut output).is_none());
    }
}
