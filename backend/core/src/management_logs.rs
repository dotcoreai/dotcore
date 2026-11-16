//! Shared management log-query inputs, admission, page size resolution, and protected-value
//! boundaries.
//!
//! An unset page size (0) takes a default, sizes up to the API's own maximum pass through
//! unchanged, and larger sizes are refused.

use std::{
    collections::BTreeMap,
    error::Error,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent, RuntimeLogQuery, RuntimeLogQueryError,
    RuntimeLogSource, RuntimeLogSourceHistory, RuntimeLogSourceStream, RuntimeLogStreamKind,
};
use futures_util::{Stream, StreamExt};
use prost::Message;
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use tracing::error;

/// Longest a manager waits for a node's log or diagnostic history, or for the first frame of a
/// follow, so a hung node cannot hold a management request.
pub(crate) const NODE_LOG_DEADLINE: Duration = Duration::from_secs(10);

/// Page size of the log and diagnostic reads when the request leaves it unset.
pub(crate) const DEFAULT_LOG_PAGE_SIZE: u32 = 100;
const MAX_CONCURRENT_MANAGEMENT_LOG_REQUESTS: usize = 8;
pub(crate) const MAX_LOG_ENTRY_CONTENT_BYTES: usize = 64 * 1024;
pub(crate) const MAX_LOG_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REDACTION_MARKER: &[u8] = b"[REDACTED]";
const MAX_LOG_DIAGNOSTIC_BYTES: usize = 4 * 1024;
const MAX_LOG_DIAGNOSTIC_SOURCES: usize = 8;

/// Composition-owned admission shared by all deployment-log services.
#[allow(
    dead_code,
    reason = "the service-owned deployment-log protocol consumes this composed admission capability"
)]
#[derive(Clone, Debug)]
pub(crate) struct ManagementLogAdmission(Arc<Semaphore>);

impl ManagementLogAdmission {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Semaphore::new(
            MAX_CONCURRENT_MANAGEMENT_LOG_REQUESTS,
        )))
    }

    #[allow(
        dead_code,
        reason = "the service-owned deployment-log protocol acquires this composed permit"
    )]
    pub(crate) fn try_acquire(&self) -> Result<OwnedSemaphorePermit, ManagementLogAdmissionError> {
        self.0
            .clone()
            .try_acquire_owned()
            .map_err(|error| match error {
                TryAcquireError::NoPermits => ManagementLogAdmissionError::Exhausted,
                TryAcquireError::Closed => ManagementLogAdmissionError::Closed,
            })
    }

    /// Waits until no request holds a permit.
    #[cfg(test)]
    pub(crate) async fn all_released(&self) {
        let all = u32::try_from(MAX_CONCURRENT_MANAGEMENT_LOG_REQUESTS).expect("permit count");
        drop(self.0.acquire_many(all).await.expect("open admission"));
    }
}

/// A validated controller request for bounded deployment-log history.
#[derive(Clone, Debug)]
pub struct DeploymentLogReadQuery {
    source_names: Vec<String>,
    history: RuntimeLogQuery,
}

impl DeploymentLogReadQuery {
    pub fn new(source_names: Vec<String>, history: RuntimeLogQuery) -> Self {
        Self {
            source_names,
            history,
        }
    }

    pub fn source_names(&self) -> &[String] {
        &self.source_names
    }

    pub fn history(&self) -> &RuntimeLogQuery {
        &self.history
    }
}

/// A validated controller request for independently resumable deployment-log streams.
#[derive(Clone, Debug)]
pub struct DeploymentLogFollowQuery {
    source_names: Vec<String>,
    resume_positions: BTreeMap<String, SystemTime>,
    cancellation: CancellationToken,
}

impl DeploymentLogFollowQuery {
    pub fn new(
        source_names: Vec<String>,
        resume_positions: BTreeMap<String, SystemTime>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            source_names,
            resume_positions,
            cancellation,
        }
    }

    pub fn source_names(&self) -> &[String] {
        &self.source_names
    }

    /// Returns the follow query of one workload group's `selection`. Resume positions of the
    /// request's other sources, such as preparation steps, are left out.
    pub fn group_query(
        &self,
        selection: &dotcore_runtime::RuntimeLogSourceSelection,
    ) -> Result<dotcore_runtime::RuntimeGroupLogFollowQuery, RuntimeLogQueryError> {
        let positions = self
            .resume_positions
            .iter()
            .filter(|(name, _)| {
                selection
                    .sources()
                    .iter()
                    .any(|source| source.name == **name)
            })
            .map(|(name, position)| (name.clone(), *position))
            .collect();
        dotcore_runtime::RuntimeGroupLogFollowQuery::new(
            selection,
            positions,
            self.cancellation.clone(),
        )
    }

    pub fn resume_positions(&self) -> &BTreeMap<String, SystemTime> {
        &self.resume_positions
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub fn source_query(&self, source_name: &str) -> dotcore_runtime::RuntimeLogFollowQuery {
        dotcore_runtime::RuntimeLogFollowQuery::new(
            self.resume_positions.get(source_name).copied(),
            self.cancellation.clone(),
        )
    }
}

/// Request-time values that must be removed from deployment-log output.
///
/// Values are distinct and kept longest first, so a value that contains a shorter one, such as a
/// password containing the username, is replaced whole before the shorter one could split it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProtectedLogValues(Vec<Vec<u8>>);

impl ProtectedLogValues {
    pub fn new(values: impl IntoIterator<Item = String>) -> Self {
        let mut protected = Self::default();
        protected.insert(values.into_iter().map(String::into_bytes));
        protected
    }

    /// Returns the values longest first.
    pub fn values(&self) -> &[Vec<u8>] {
        &self.0
    }

    /// Adds the values of `other` that are not protected yet.
    pub(crate) fn extend(&mut self, other: ProtectedLogValues) {
        self.insert(other.0);
    }

    fn insert(&mut self, values: impl IntoIterator<Item = Vec<u8>>) {
        self.0
            .extend(values.into_iter().filter(|value| !value.is_empty()));
        self.0
            .sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
        self.0.dedup();
    }

    /// Returns `text` with every protected value replaced by the redaction marker, for free text
    /// such as a recorded failure that may quote a launch value.
    pub(crate) fn redact(&self, text: &str) -> String {
        let marker = String::from_utf8_lossy(REDACTION_MARKER);
        self.0
            .iter()
            .filter_map(|value| std::str::from_utf8(value).ok())
            .fold(text.to_owned(), |text, value| text.replace(value, &marker))
    }
}

/// Stands in for the value of a sensitive launch flag in recorded custom arguments.
const MASKED_ARGUMENT_VALUE: &str = "<redacted>";

/// Masks the value of every long flag whose name contains a sensitive name as whole words, in the
/// `--flag value` and `--flag=value` forms, keeping the flag and the token order. Best effort:
/// short aliases and secrets without a sensitive flag name are not detected.
pub(crate) fn mask_sensitive_arguments(tokens: &[String]) -> Vec<String> {
    let mut masked = Vec::with_capacity(tokens.len());
    let mut mask_next = false;
    for token in tokens {
        if std::mem::take(&mut mask_next) && !token.starts_with("--") {
            masked.push(MASKED_ARGUMENT_VALUE.to_owned());
            continue;
        }
        let Some(flag) = token.strip_prefix("--") else {
            masked.push(token.clone());
            continue;
        };
        let (name, has_value) = match flag.split_once('=') {
            Some((name, _)) => (name, true),
            None => (flag, false),
        };
        if !dotcore_catalog::is_sensitive_name(name) {
            masked.push(token.clone());
        } else if has_value {
            masked.push(format!("--{name}={MASKED_ARGUMENT_VALUE}"));
        } else {
            masked.push(token.clone());
            mask_next = true;
        }
    }
    masked
}

/// Public-safe context attached to one internal deployment-log diagnostic.
#[derive(Clone, Debug)]
pub(crate) struct DeploymentLogDiagnosticContext {
    owner_kind: &'static str,
    deployment_id: String,
    operation: &'static str,
}

impl DeploymentLogDiagnosticContext {
    pub(crate) fn new(
        owner_kind: &'static str,
        deployment_id: impl Into<String>,
        operation: &'static str,
    ) -> Self {
        Self {
            owner_kind,
            deployment_id: deployment_id.into(),
            operation,
        }
    }
}

/// Shared controller-log boundary failures without runtime-native identity.
#[derive(Debug, thiserror::Error)]
pub enum DeploymentLogError {
    #[error("deployment log request contains an invalid source")]
    InvalidSource,

    #[error("deployment log deployment was not found")]
    NotFound,

    #[error("deployment has no local runtime log source")]
    UnsupportedDeployment,

    /// The node hosting the deployment cannot serve its logs right now, for example while it is
    /// unreachable, slow, or has not installed the deployment yet.
    #[error("deployment log node is unavailable")]
    NodeUnavailable,

    #[error("deployment log owner resolution failed")]
    Configuration(#[source] anyhow::Error),
}

impl DeploymentLogError {
    pub(crate) fn configuration(source: impl Into<anyhow::Error>) -> Self {
        Self::Configuration(
            source
                .into()
                .context("deployment log owner resolution failed"),
        )
    }
}

/// Failure to admit another management deployment-log request.
#[allow(
    dead_code,
    reason = "the service-owned deployment-log protocol maps admission failures to transport status"
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ManagementLogAdmissionError {
    #[error("management deployment-log request capacity is exhausted")]
    Exhausted,

    #[error("management deployment-log admission is closed")]
    Closed,
}

/// Public-safe point-in-time source outcome prepared for an owner service response.
#[derive(Debug)]
pub(crate) struct DeploymentLogHistory {
    pub sources: Vec<DeploymentLogSourceStatus>,
    pub entries: Vec<RuntimeLogEntry>,
    pub latest_positions: BTreeMap<String, SystemTime>,
    pub older_output_may_remain: bool,
    pub response_truncated: bool,
    pub next_before_timestamp: Option<SystemTime>,
}

/// Public-safe source state used by history and follow response mapping.
#[derive(Clone, Debug)]
pub(crate) struct DeploymentLogSourceStatus {
    pub source: RuntimeLogSource,
    pub available: bool,
}

/// Merges independently acquired source histories into one bounded aggregate page.
pub(crate) fn aggregate_history(
    histories: Vec<RuntimeLogSourceHistory>,
    page_size: usize,
    protected: &ProtectedLogValues,
    diagnostic_context: &DeploymentLogDiagnosticContext,
) -> DeploymentLogHistory {
    let mut sources = Vec::with_capacity(histories.len());
    let mut entries = Vec::new();
    let mut older_output_may_remain = false;
    for history in histories {
        let source = history.source;
        match history.result {
            Ok(history) => {
                older_output_may_remain |=
                    history.has_older_output || history.candidate_budget_truncated;
                sources.push(DeploymentLogSourceStatus {
                    source,
                    available: true,
                });
                entries.extend(history.entries);
            }
            Err(error) => {
                record_source_failure(diagnostic_context, Some(&source), &error, protected);
                sources.push(DeploymentLogSourceStatus {
                    source,
                    available: false,
                });
            }
        }
    }
    entries.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.source.name.cmp(&right.source.name))
    });

    // Resume positions cover every source read, so a follow doesn't replay the lines of a source
    // that the page trims below.
    let latest_positions = latest_positions(&entries);
    if entries.len() > page_size {
        older_output_may_remain = true;
        entries.drain(..entries.len() - page_size);
    }
    filter_entries(&mut entries, protected);
    let mut response_truncated = false;
    while !entries.is_empty()
        && encoded_history_response_bytes(
            &sources,
            &entries,
            &latest_positions,
            older_output_may_remain,
            response_truncated,
        ) > MAX_LOG_RESPONSE_BYTES
    {
        response_truncated = true;
        older_output_may_remain = true;
        entries.remove(0);
    }
    let next_before_timestamp = entries
        .first()
        .map(|entry| whole_second_before(entry.timestamp));
    DeploymentLogHistory {
        sources,
        entries,
        latest_positions,
        older_output_may_remain,
        response_truncated,
        next_before_timestamp,
    }
}

/// Filters owner-protected byte values before any record reaches a management response.
///
/// An entry the runtime already truncated may end inside a protected value, for example a line
/// cut at 64 KiB just after the first bytes of an API key, so a trailing partial value is
/// redacted too.
pub(crate) fn filter_entry(
    mut entry: RuntimeLogEntry,
    protected: &ProtectedLogValues,
) -> RuntimeLogEntry {
    // The trailing part goes first, so a shorter value it contains can't split it.
    if entry.truncated {
        redact_trailing_partial_value(&mut entry.content, protected);
    }
    for value in protected.values() {
        entry.content = filter_value(&entry.content, value, &mut entry.truncated);
    }
    if entry.content.len() > MAX_LOG_ENTRY_CONTENT_BYTES {
        entry.content.truncate(MAX_LOG_ENTRY_CONTENT_BYTES);
        entry.truncated = true;
    }
    entry
}

/// Logs one unexpected source failure with protected values filtered from its cause; expected
/// outcomes, such as a workload that does not exist yet, are not logged.
pub(crate) fn record_source_failure(
    context: &DeploymentLogDiagnosticContext,
    source: Option<&RuntimeLogSource>,
    failure: &(dyn Error + 'static),
    protected: &ProtectedLogValues,
) {
    if is_expected_log_outcome(failure) {
        return;
    }
    let cause = filtered_error_chain(failure, protected);
    let source_name = source.map_or("unknown", |source| source.name.as_str());
    error!(
        operation = %context.operation,
        owner_kind = %context.owner_kind,
        deployment_id = %context.deployment_id,
        source_name = %source_name,
        cause = %cause,
        "Deployment log acquisition failed"
    );
}

fn is_expected_log_outcome(failure: &(dyn Error + 'static)) -> bool {
    error_sources(failure).any(|source| {
        source
            .downcast_ref::<RuntimeBackendError>()
            .is_some_and(|error| {
                matches!(
                    error,
                    RuntimeBackendError::LogCapabilityUnavailable
                        | RuntimeBackendError::WorkloadNotCreated
                        | RuntimeBackendError::LogCancelled
                        | RuntimeBackendError::RemoteLogUnavailable
                )
            })
    })
}

fn filtered_error_chain(failure: &(dyn Error + 'static), protected: &ProtectedLogValues) -> String {
    let sources = error_sources(failure)
        .take(MAX_LOG_DIAGNOSTIC_SOURCES)
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut rendered = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let distinct = sources
            .get(index + 1)
            .and_then(|next| source.strip_suffix(next))
            .map(|context| context.trim_end_matches(": "))
            .unwrap_or(source);
        if !distinct.is_empty() {
            rendered.push(distinct);
        }
    }
    let mut filtered = rendered.join(": ").into_bytes();
    let mut truncated = false;
    for value in protected.values() {
        filtered = filter_value(&filtered, value, &mut truncated);
    }
    let mut rendered = String::from_utf8_lossy(&filtered).into_owned();
    if rendered.len() > MAX_LOG_DIAGNOSTIC_BYTES {
        let mut end = MAX_LOG_DIAGNOSTIC_BYTES;
        while !rendered.is_char_boundary(end) {
            end -= 1;
        }
        rendered.truncate(end);
    }
    rendered
}

fn error_sources<'a>(
    failure: &'a (dyn Error + 'static),
) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    std::iter::successors(Some(failure), |source| (*source).source())
}

/// The streams of one deployment-log follow: the sources open from the start, the sources that
/// start while it runs, such as a preparation step, and whether the follow failed.
pub struct DeploymentLogStreams {
    pub sources: Vec<RuntimeLogSourceStream>,

    /// Sources that start after the follow opened; absent when none can.
    pub late: Option<tokio::sync::mpsc::Receiver<RuntimeLogSourceStream>>,

    /// Set before the source streams end when the follow failed rather than ended.
    pub failure: DeploymentLogFollowFailure,
}

impl DeploymentLogStreams {
    /// Streams of a follow whose sources are all known when it opens.
    pub fn fixed(sources: Vec<RuntimeLogSourceStream>) -> Self {
        Self {
            sources,
            late: None,
            failure: DeploymentLogFollowFailure::default(),
        }
    }
}

/// Whether a deployment-log follow failed, such as when the connection to its node broke.
///
/// The producer marks it before ending its source streams, so a source whose stream then ends
/// was cut short, not ended, and the whole follow ends with an error instead of cleanly.
#[derive(Clone, Debug, Default)]
pub struct DeploymentLogFollowFailure(Arc<OnceLock<()>>);

impl DeploymentLogFollowFailure {
    /// Marks the follow as failed.
    pub(crate) fn fail(&self) {
        let _ = self.0.set(());
    }

    pub(crate) fn failed(&self) -> bool {
        self.0.get().is_some()
    }
}

/// Builds the response stream of one management follow: its public events in per-source order,
/// mapped by `map_event`, ending cleanly when the follow ends and with an error status when it
/// fails. The stream holds `permit` and cancels the follow when dropped.
pub(crate) fn follow_response<T: Send + 'static>(
    streams: DeploymentLogStreams,
    protected: ProtectedLogValues,
    diagnostic_context: DeploymentLogDiagnosticContext,
    cancellation: CancellationToken,
    permit: OwnedSemaphorePermit,
    map_event: fn(DeploymentLogFollowEvent) -> T,
) -> Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send>> {
    let events = follow_sources(streams, protected, cancellation.clone(), diagnostic_context).map(
        move |event| {
            // The admission permit lives as long as the follow.
            let _permit = &permit;
            event.map(map_event)
        },
    );
    Box::pin(CancellableLogStream::new(Box::pin(events), cancellation))
}

/// Fans source streams into a single bounded, cancellation-aware management stream. A source that
/// starts while the follow runs is announced and followed like the others, and one that starts
/// again waits until its earlier stream is forwarded, so events of one source stay in order. A
/// failed follow ends with an error status after the events already received.
pub(crate) fn follow_sources(
    streams: DeploymentLogStreams,
    protected: ProtectedLogValues,
    cancellation: CancellationToken,
    diagnostic_context: DeploymentLogDiagnosticContext,
) -> impl Stream<Item = Result<DeploymentLogFollowEvent, tonic::Status>> + Send {
    let DeploymentLogStreams {
        sources,
        late,
        failure,
    } = streams;
    let (sender, receiver) = tokio::sync::mpsc::channel(256);
    let forward = SourceForward {
        sender,
        protected,
        cancellation,
        diagnostic_context,
        failure: failure.clone(),
    };
    let mut running = BTreeMap::new();
    for source_stream in sources {
        forward.spawn_in_order(&mut running, source_stream);
    }
    if let Some(mut late) = late {
        let forward = forward.clone();
        tokio::spawn(async move {
            loop {
                let source_stream = tokio::select! {
                    () = forward.cancellation.cancelled() => return,
                    source_stream = late.recv() => source_stream,
                };
                let Some(source_stream) = source_stream else {
                    return;
                };
                forward.spawn_in_order(&mut running, source_stream);
            }
        });
    }
    drop(forward);
    // Every forwarder is done once the receiver ends, so the failure is final by then.
    let end = futures_util::stream::once(async move {
        failure.failed().then(|| {
            Err(tonic::Status::unavailable(
                "Deployment log follow was interrupted; reopen it to resume",
            ))
        })
    })
    .filter_map(std::future::ready);
    tokio_stream::wrappers::ReceiverStream::new(receiver)
        .map(Ok)
        .chain(end)
}

/// Forwards one source's events into a follow's management stream.
#[derive(Clone)]
struct SourceForward {
    sender: tokio::sync::mpsc::Sender<DeploymentLogFollowEvent>,
    protected: ProtectedLogValues,
    cancellation: CancellationToken,
    diagnostic_context: DeploymentLogDiagnosticContext,
    failure: DeploymentLogFollowFailure,
}

impl SourceForward {
    /// Forwards `source_stream` once the forwarder of its source's earlier stream, if any, is
    /// done, and records it as that source's latest forwarder in `running`.
    fn spawn_in_order(
        &self,
        running: &mut BTreeMap<String, JoinHandle<()>>,
        source_stream: RuntimeLogSourceStream,
    ) {
        let earlier = running.remove(&source_stream.source.name);
        let name = source_stream.source.name.clone();
        let forward = self.clone();
        let forwarder = tokio::spawn(async move {
            if let Some(earlier) = earlier {
                // A forwarder that panicked is done too.
                let _ = earlier.await;
            }
            forward.run(source_stream).await;
        });
        running.insert(name, forwarder);
    }

    async fn run(self, source_stream: RuntimeLogSourceStream) {
        let Self {
            sender,
            protected,
            cancellation,
            diagnostic_context,
            failure,
        } = self;
        let source = source_stream.source;
        let mut stream = match source_stream.result {
            Ok(stream) => stream,
            Err(error) => {
                record_source_failure(&diagnostic_context, Some(&source), &error, &protected);
                send_follow_event(
                    &sender,
                    DeploymentLogFollowEvent::Unavailable(source),
                    &cancellation,
                )
                .await;
                return;
            }
        };
        if !send_follow_event(
            &sender,
            DeploymentLogFollowEvent::Available(source.clone()),
            &cancellation,
        )
        .await
        {
            return;
        }
        loop {
            let event = tokio::select! {
                () = cancellation.cancelled() => return,
                event = stream.next() => event,
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Ok(RuntimeLogEvent::Entry(entry)) => {
                    let event = DeploymentLogFollowEvent::Entry(filter_entry(entry, &protected));
                    if !send_follow_event(&sender, event, &cancellation).await {
                        return;
                    }
                }
                Ok(RuntimeLogEvent::SourceUnavailable(source)) => {
                    send_follow_event(
                        &sender,
                        DeploymentLogFollowEvent::Unavailable(source),
                        &cancellation,
                    )
                    .await;
                    return;
                }
                Err(error) => {
                    record_source_failure(&diagnostic_context, Some(&source), &error, &protected);
                    send_follow_event(
                        &sender,
                        DeploymentLogFollowEvent::Unavailable(source),
                        &cancellation,
                    )
                    .await;
                    return;
                }
            }
        }
        // A stream cut short by a failed follow did not end; the follow's error says so.
        if failure.failed() {
            return;
        }
        send_follow_event(
            &sender,
            DeploymentLogFollowEvent::Ended(source),
            &cancellation,
        )
        .await;
    }
}

async fn send_follow_event(
    sender: &tokio::sync::mpsc::Sender<DeploymentLogFollowEvent>,
    event: DeploymentLogFollowEvent,
    cancellation: &CancellationToken,
) -> bool {
    tokio::select! {
        () = cancellation.cancelled() => false,
        result = sender.send(event) => result.is_ok(),
    }
}

/// Cancels runtime acquisition when the transport drops its follow response stream.
pub(crate) struct CancellableLogStream<S> {
    inner: S,
    cancellation: CancellationToken,
}

impl<S> CancellableLogStream<S> {
    pub(crate) fn new(inner: S, cancellation: CancellationToken) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl<S: Stream + Unpin> Stream for CancellableLogStream<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.inner).poll_next(context)
    }
}

impl<S> Drop for CancellableLogStream<S> {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// One public-safe fan-in item.
#[derive(Debug)]
pub(crate) enum DeploymentLogFollowEvent {
    Available(RuntimeLogSource),
    Unavailable(RuntimeLogSource),
    Ended(RuntimeLogSource),
    Entry(RuntimeLogEntry),
}

pub(crate) fn map_runtime_error_with_diagnostic(
    error: DeploymentLogError,
    diagnostic_context: &DeploymentLogDiagnosticContext,
    protected: Option<&ProtectedLogValues>,
) -> tonic::Status {
    if let DeploymentLogError::Configuration(failure) = &error {
        if let Some(protected) = protected {
            record_source_failure(diagnostic_context, None, failure.as_ref(), protected);
        } else if !is_expected_log_outcome(failure.as_ref()) {
            error!(
                operation = %diagnostic_context.operation,
                owner_kind = %diagnostic_context.owner_kind,
                deployment_id = %diagnostic_context.deployment_id,
                "Deployment log preparation failed before protected values were available"
            );
        }
    }
    map_runtime_error(error)
}

pub(crate) fn map_runtime_error(error: DeploymentLogError) -> tonic::Status {
    match error {
        DeploymentLogError::InvalidSource => {
            tonic::Status::invalid_argument("Deployment log request contains an invalid source")
        }
        DeploymentLogError::NotFound => {
            tonic::Status::not_found("Deployment log deployment was not found")
        }
        DeploymentLogError::UnsupportedDeployment | DeploymentLogError::Configuration(_) => {
            tonic::Status::failed_precondition(
                "Deployment logs are not available for this deployment",
            )
        }
        DeploymentLogError::NodeUnavailable => {
            tonic::Status::unavailable("Deployment logs are unavailable right now; retry shortly")
        }
    }
}

pub(crate) fn map_admission_error(_: ManagementLogAdmissionError) -> tonic::Status {
    tonic::Status::resource_exhausted("Deployment log request capacity is exhausted; retry shortly")
}

/// Returns the effective page size, or `None` when `requested` exceeds `maximum`. A requested
/// size of 0 selects `default`.
pub(crate) fn resolve_page_size(requested: u32, default: u32, maximum: u32) -> Option<u32> {
    match requested {
        0 => Some(default),
        requested if requested <= maximum => Some(requested),
        _ => None,
    }
}

fn filter_entries(entries: &mut [RuntimeLogEntry], protected: &ProtectedLogValues) {
    for entry in entries {
        *entry = filter_entry(entry.clone(), protected);
    }
}

fn filter_value(content: &[u8], value: &[u8], truncated: &mut bool) -> Vec<u8> {
    if value.is_empty() {
        return content[..content.len().min(MAX_LOG_ENTRY_CONTENT_BYTES)].to_vec();
    }
    let mut filtered = Vec::with_capacity(content.len().min(MAX_LOG_ENTRY_CONTENT_BYTES));
    let mut remainder = content;
    while let Some(offset) = remainder
        .windows(value.len())
        .position(|window| window == value)
    {
        if !append_bounded(&mut filtered, &remainder[..offset])
            || !append_bounded(&mut filtered, REDACTION_MARKER)
        {
            *truncated = true;
            return filtered;
        }
        remainder = &remainder[offset + value.len()..];
    }
    if !append_bounded(&mut filtered, remainder) {
        *truncated = true;
    }
    filtered
}

/// Replaces the longest suffix of `content` that begins a protected value with the marker.
fn redact_trailing_partial_value(content: &mut Vec<u8>, protected: &ProtectedLogValues) {
    let partial = protected
        .values()
        .iter()
        .filter_map(|value| {
            (1..value.len().min(content.len() + 1))
                .rev()
                .find(|length| content.ends_with(&value[..*length]))
        })
        .max();
    if let Some(length) = partial {
        content.truncate(content.len() - length);
        content.extend_from_slice(REDACTION_MARKER);
    }
}

fn append_bounded(filtered: &mut Vec<u8>, bytes: &[u8]) -> bool {
    let remaining = MAX_LOG_ENTRY_CONTENT_BYTES.saturating_sub(filtered.len());
    let retained = bytes.len().min(remaining);
    filtered.extend_from_slice(&bytes[..retained]);
    retained == bytes.len()
}

fn latest_positions(entries: &[RuntimeLogEntry]) -> BTreeMap<String, SystemTime> {
    let mut positions: BTreeMap<String, SystemTime> = BTreeMap::new();
    for entry in entries {
        positions
            .entry(entry.source.name.clone())
            .and_modify(|timestamp| *timestamp = (*timestamp).max(entry.timestamp))
            .or_insert(entry.timestamp);
    }
    positions
}

fn whole_second_before(timestamp: SystemTime) -> SystemTime {
    let seconds = timestamp
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    UNIX_EPOCH + std::time::Duration::from_secs(seconds.saturating_sub(1))
}

fn encoded_history_response_bytes(
    sources: &[DeploymentLogSourceStatus],
    entries: &[RuntimeLogEntry],
    latest_positions: &BTreeMap<String, SystemTime>,
    older_output_may_remain: bool,
    response_truncated: bool,
) -> usize {
    use dotcore_proto::dotcore::model::v1::{
        DeploymentLogHistoryState, DeploymentLogResumePosition, DeploymentLogSource,
        DeploymentLogSourceState, ReadDeploymentLogsResponse,
    };

    ReadDeploymentLogsResponse {
        sources: sources
            .iter()
            .map(|source| DeploymentLogSource {
                source_name: source.source.name.clone(),
                source_label: source.source.label.clone(),
                state: if source.available {
                    DeploymentLogSourceState::Available as i32
                } else {
                    DeploymentLogSourceState::Unavailable as i32
                },
            })
            .collect(),
        entries: entries.iter().cloned().map(model_log_entry).collect(),
        next_before_timestamp: entries
            .first()
            .map(|entry| protobuf_timestamp(whole_second_before(entry.timestamp))),
        history_state: if older_output_may_remain {
            DeploymentLogHistoryState::OlderOutputMayRemain as i32
        } else {
            DeploymentLogHistoryState::NoOlderOutputObserved as i32
        },
        response_truncated,
        latest_positions: latest_positions
            .iter()
            .map(|(source_name, timestamp)| DeploymentLogResumePosition {
                source_name: source_name.clone(),
                latest_timestamp: Some(protobuf_timestamp(*timestamp)),
            })
            .collect(),
    }
    .encoded_len()
}

fn model_log_entry(
    entry: RuntimeLogEntry,
) -> dotcore_proto::dotcore::model::v1::DeploymentLogEntry {
    use dotcore_proto::dotcore::model::v1::DeploymentLogStream;

    dotcore_proto::dotcore::model::v1::DeploymentLogEntry {
        source_name: entry.source.name,
        source_label: entry.source.label,
        stream: match entry.stream {
            RuntimeLogStreamKind::Stdout => DeploymentLogStream::Stdout as i32,
            RuntimeLogStreamKind::Stderr => DeploymentLogStream::Stderr as i32,
        },
        timestamp: Some(protobuf_timestamp(entry.timestamp)),
        content: entry.content,
        truncated: entry.truncated,
    }
}

fn protobuf_timestamp(time: SystemTime) -> dotcore_proto::google::protobuf::Timestamp {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    dotcore_proto::google::protobuf::Timestamp {
        seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
        nanos: duration.subsec_nanos() as i32,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dotcore_domain::test_support::tracing::{Record, capture_records, capture_records_async};
    use dotcore_runtime::{RuntimeBackendError, RuntimeLogHistory, RuntimeLogStreamKind};
    use futures_util::StreamExt;

    use super::*;

    #[test]
    fn masks_the_value_after_a_sensitive_flag_and_keeps_other_tokens_in_order() {
        let tokens = |tokens: &[&str]| tokens.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
        let cases = [
            (
                tokens(&["--api-key", "sk-1", "--seed", "7"]),
                tokens(&["--api-key", "<redacted>", "--seed", "7"]),
            ),
            (
                tokens(&["--seed", "7", "--api-key=sk-1"]),
                tokens(&["--seed", "7", "--api-key=<redacted>"]),
            ),
            (
                tokens(&["--hf-token", "x"]),
                tokens(&["--hf-token", "<redacted>"]),
            ),
            (
                tokens(&["--cache-type-k", "q8_0", "--max-num-batched-tokens", "8192"]),
                tokens(&["--cache-type-k", "q8_0", "--max-num-batched-tokens", "8192"]),
            ),
            (tokens(&["--api-key"]), tokens(&["--api-key"])),
            (
                tokens(&["--api-key", "--seed", "7"]),
                tokens(&["--api-key", "--seed", "7"]),
            ),
        ];

        for (input, expected) in cases {
            assert_eq!(mask_sensitive_arguments(&input), expected, "{input:?}");
        }
    }

    #[test]
    fn unset_page_size_takes_the_default_and_bounded_sizes_pass_through() {
        for (requested, expected) in [
            (0, Some(DEFAULT_LOG_PAGE_SIZE)),
            (1, Some(1)),
            (100, Some(100)),
            (200, Some(200)),
            (201, None),
            (u32::MAX, None),
        ] {
            assert_eq!(
                resolve_page_size(requested, DEFAULT_LOG_PAGE_SIZE, 200),
                expected,
                "{requested}"
            );
        }
    }

    #[test]
    fn aggregate_orders_newest_page_filters_values_and_preserves_whole_second_caveat() {
        let alpha = source("alpha");
        let beta = source("beta");
        let timestamp = UNIX_EPOCH + Duration::from_secs(20) + Duration::from_nanos(3);
        let history = aggregate_history(
            vec![
                history(alpha.clone(), vec![entry(alpha.clone(), 19, b"old")], true),
                history(
                    beta.clone(),
                    vec![
                        entry(beta.clone(), 20, b"token token"),
                        RuntimeLogEntry {
                            timestamp,
                            ..entry(alpha.clone(), 20, b"second")
                        },
                    ],
                    false,
                ),
            ],
            2,
            &ProtectedLogValues::new(["token".to_owned()]),
            &diagnostic_context("read_history"),
        );

        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| (&entry.source.name, &entry.content))
                .collect::<Vec<_>>(),
            vec![
                (&"beta".to_owned(), &b"[REDACTED] [REDACTED]".to_vec()),
                (&"alpha".to_owned(), &b"second".to_vec()),
            ]
        );
        assert!(history.older_output_may_remain);
        assert!(!history.response_truncated);
        assert_eq!(
            history.latest_positions,
            BTreeMap::from([
                ("alpha".to_owned(), timestamp),
                ("beta".to_owned(), UNIX_EPOCH + Duration::from_secs(20)),
            ])
        );
        assert_eq!(
            history.next_before_timestamp,
            Some(UNIX_EPOCH + Duration::from_secs(19))
        );
    }

    #[test]
    fn aggregate_resumes_a_source_whose_lines_were_all_trimmed_from_the_page() {
        let alpha = source("alpha");
        let beta = source("beta");
        let history = aggregate_history(
            vec![
                history(
                    alpha.clone(),
                    vec![
                        entry(alpha.clone(), 1, b"older"),
                        entry(alpha.clone(), 2, b"old"),
                    ],
                    false,
                ),
                history(
                    beta.clone(),
                    vec![
                        entry(beta.clone(), 10, b"new"),
                        entry(beta.clone(), 11, b"newer"),
                    ],
                    false,
                ),
            ],
            2,
            &ProtectedLogValues::default(),
            &diagnostic_context("read_history"),
        );

        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| &entry.content)
                .collect::<Vec<_>>(),
            vec![&b"new".to_vec(), &b"newer".to_vec()]
        );
        assert_eq!(
            history.latest_positions,
            BTreeMap::from([
                ("alpha".to_owned(), UNIX_EPOCH + Duration::from_secs(2)),
                ("beta".to_owned(), UNIX_EPOCH + Duration::from_secs(11)),
            ])
        );
    }

    #[test]
    fn aggregate_enforces_exact_200_record_and_four_mib_response_bounds() {
        let source = source("web");
        let entries = (0..201)
            .map(|index| entry(source.clone(), index, format!("line-{index}").as_bytes()))
            .collect();
        let count_bounded = aggregate_history(
            vec![history(source.clone(), entries, false)],
            200,
            &ProtectedLogValues::default(),
            &diagnostic_context("read_history"),
        );
        assert_eq!(count_bounded.entries.len(), 200);
        assert_eq!(count_bounded.entries[0].content, b"line-1");
        assert_eq!(count_bounded.entries[199].content, b"line-200");
        assert!(count_bounded.older_output_may_remain);
        assert!(!count_bounded.response_truncated);

        let payload = vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES];
        let byte_bounded = aggregate_history(
            vec![history(
                source.clone(),
                (0..65)
                    .map(|index| entry(source.clone(), index, &payload))
                    .collect(),
                false,
            )],
            200,
            &ProtectedLogValues::default(),
            &diagnostic_context("read_history"),
        );
        assert_eq!(byte_bounded.entries.len(), 63);
        assert_eq!(
            byte_bounded.entries[0].timestamp,
            UNIX_EPOCH + Duration::from_secs(2)
        );
        assert!(byte_bounded.older_output_may_remain);
        assert!(byte_bounded.response_truncated);
    }

    #[test]
    fn a_protected_value_cut_by_the_runtime_is_redacted_on_the_local_path() {
        const SECRET: &str = "launch-secret-0123456789";
        let source = source("web");
        let mut cut = vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES - 6];
        cut.extend_from_slice(&SECRET.as_bytes()[..6]);
        let runtime_cut = RuntimeLogEntry {
            truncated: true,
            ..entry(source.clone(), 1, &cut)
        };
        let complete = entry(source.clone(), 2, b"ends with launc");

        let history = aggregate_history(
            vec![history(source, vec![runtime_cut, complete], false)],
            200,
            &ProtectedLogValues::new([SECRET.to_owned()]),
            &diagnostic_context("read_history"),
        );

        let redacted = &history.entries[0];
        assert_eq!(redacted.content.len(), MAX_LOG_ENTRY_CONTENT_BYTES);
        assert_eq!(
            String::from_utf8_lossy(&redacted.content[MAX_LOG_ENTRY_CONTENT_BYTES - 10..]),
            "xxxx[REDAC",
            "the cut value's first bytes are replaced by the marker"
        );
        assert!(redacted.truncated);
        assert_eq!(
            history.entries[1].content, b"ends with launc",
            "a complete entry ending like a value is left as is"
        );
    }

    #[test]
    fn a_shorter_protected_value_never_splits_a_longer_one() {
        // The shorter value comes first, as a username listed before the password would.
        let protected = ProtectedLogValues::new([
            "admin".to_owned(),
            "admin-Sup3rPass".to_owned(),
            "admin".to_owned(),
        ]);
        assert_eq!(
            protected.values(),
            [b"admin-Sup3rPass".to_vec(), b"admin".to_vec()]
        );

        let filtered = filter_entry(
            entry(source("web"), 1, b"login admin admin-Sup3rPass"),
            &protected,
        );
        assert_eq!(filtered.content, b"login [REDACTED] [REDACTED]");
        assert_eq!(
            protected.redact("rejected admin-Sup3rPass for admin"),
            "rejected [REDACTED] for [REDACTED]"
        );

        let mut cut = entry(source("web"), 2, b"login admin admin-Sup");
        cut.truncated = true;
        assert_eq!(
            filter_entry(cut, &protected).content,
            b"login [REDACTED] [REDACTED]"
        );

        let mut extended = ProtectedLogValues::new(["1".to_owned()]);
        extended.extend(ProtectedLogValues::new(["key-1-secret".to_owned()]));
        assert_eq!(extended.redact("key-1-secret"), "[REDACTED]");
    }

    #[test]
    fn empty_protected_values_still_bound_entry_content() {
        let filtered = filter_entry(
            entry(
                source("web"),
                1,
                &vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES + 1],
            ),
            &ProtectedLogValues::default(),
        );

        assert_eq!(filtered.content.len(), MAX_LOG_ENTRY_CONTENT_BYTES);
        assert!(filtered.truncated);
    }

    #[test]
    fn aggregate_bounds_complete_protobuf_response_with_metadata() {
        let metadata_sources = (0..32)
            .map(|index| RuntimeLogSource {
                name: format!("source-{index}"),
                label: "m".repeat(100_000),
            })
            .collect::<Vec<_>>();
        let entry_source = metadata_sources[0].clone();
        let history = aggregate_history(
            metadata_sources
                .into_iter()
                .enumerate()
                .map(|(index, source)| {
                    history(
                        source.clone(),
                        if index == 0 {
                            (0..20)
                                .map(|seconds| entry(source.clone(), seconds, &vec![b'x'; 65_000]))
                                .collect()
                        } else {
                            Vec::new()
                        },
                        false,
                    )
                })
                .collect(),
            200,
            &ProtectedLogValues::default(),
            &diagnostic_context("read_history"),
        );

        assert!(history.entries.len() < 20);
        assert!(history.response_truncated);
        assert!(history.older_output_may_remain);
        assert_eq!(
            history.next_before_timestamp,
            history
                .entries
                .first()
                .map(|entry| whole_second_before(entry.timestamp))
        );
        assert!(
            encoded_history_response_bytes(
                &history.sources,
                &history.entries,
                &history.latest_positions,
                history.older_output_may_remain,
                history.response_truncated,
            ) <= MAX_LOG_RESPONSE_BYTES
        );
        assert_eq!(history.entries.last().unwrap().source, entry_source);
    }

    #[test]
    fn redaction_bounds_expanding_values_before_record_and_response_sizing() {
        let source = source("web");
        let protected = ProtectedLogValues::new(["x".to_owned()]);
        let filtered = filter_entry(
            entry(
                source.clone(),
                1,
                &vec![b'x'; 8 * MAX_LOG_ENTRY_CONTENT_BYTES],
            ),
            &protected,
        );
        assert_eq!(filtered.content.len(), MAX_LOG_ENTRY_CONTENT_BYTES);
        assert!(filtered.truncated);
        assert!(!filtered.content.windows(1).any(|window| window == b"x"));

        let history = aggregate_history(
            vec![history(
                source.clone(),
                (0..65)
                    .map(|index| {
                        entry(
                            source.clone(),
                            index,
                            &vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES],
                        )
                    })
                    .collect(),
                false,
            )],
            200,
            &protected,
            &diagnostic_context("read_history"),
        );
        assert!(
            encoded_history_response_bytes(
                &history.sources,
                &history.entries,
                &history.latest_positions,
                history.older_output_may_remain,
                history.response_truncated,
            ) <= MAX_LOG_RESPONSE_BYTES
        );
        assert!(history.response_truncated);
    }

    #[tokio::test]
    async fn fan_in_is_bounded_and_isolates_source_failure_and_eof() {
        let healthy = source("healthy");
        let unavailable = source("unavailable");
        let ended = source("ended");
        let streams = vec![
            RuntimeLogSourceStream {
                source: healthy.clone(),
                result: Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                    RuntimeLogEvent::Entry(entry(healthy.clone(), 1, b"private")),
                )]))),
            },
            RuntimeLogSourceStream {
                source: unavailable.clone(),
                result: Err(RuntimeBackendError::log_capability_unavailable()),
            },
            RuntimeLogSourceStream {
                source: ended.clone(),
                result: Ok(Box::pin(futures_util::stream::empty())),
            },
        ];
        let events = follow_sources(
            DeploymentLogStreams::fixed(streams),
            ProtectedLogValues::new(["private".to_owned()]),
            CancellationToken::new(),
            diagnostic_context("follow"),
        )
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|event| event.expect("fan-in event"))
        .collect::<Vec<_>>();

        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DeploymentLogFollowEvent::Unavailable(source) if source.name == "unavailable"))
                .count(),
            1
        );
        assert!(!events.iter().any(|event| matches!(event, DeploymentLogFollowEvent::Ended(source) if source.name == "unavailable")));
        assert!(events.iter().any(|event| matches!(event, DeploymentLogFollowEvent::Ended(source) if source.name == "ended")));
        assert!(events.iter().any(|event| matches!(event, DeploymentLogFollowEvent::Entry(entry) if entry.content == b"[REDACTED]")));
    }

    #[tokio::test]
    async fn cancellation_stops_a_full_fan_in_without_waiting_for_receiver_capacity() {
        let cancellation = CancellationToken::new();
        let source = source("busy");
        let stream = futures_util::stream::unfold(source.clone(), |source| async move {
            Some((
                Ok(RuntimeLogEvent::Entry(entry(source.clone(), 1, b"line"))),
                source,
            ))
        });
        let output = follow_sources(
            DeploymentLogStreams::fixed(vec![RuntimeLogSourceStream {
                source,
                result: Ok(Box::pin(stream)),
            }]),
            ProtectedLogValues::default(),
            cancellation.clone(),
            diagnostic_context("follow"),
        );
        tokio::pin!(output);
        for _ in 0..256 {
            output
                .next()
                .await
                .expect("bounded queued event")
                .expect("fan-in event");
        }
        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), output.next())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn real_follow_failure_emits_unavailable_once_and_one_filtered_diagnostic() {
        let source = source("web");
        let secret = "private-token";
        let stream =
            futures_util::stream::iter(vec![Err(RuntimeBackendError::docker_with_source(
                "following Docker logs",
                std::io::Error::other(format!("daemon rejected {secret}")),
            ))]);
        let (events, records, _) = capture_records_async(|| async {
            follow_sources(
                DeploymentLogStreams::fixed(vec![RuntimeLogSourceStream {
                    source,
                    result: Ok(Box::pin(stream)),
                }]),
                ProtectedLogValues::new([secret.to_owned()]),
                CancellationToken::new(),
                diagnostic_context("follow"),
            )
            .collect::<Vec<_>>()
            .await
        })
        .await;
        let events = events
            .into_iter()
            .map(|event| event.expect("fan-in event"))
            .collect::<Vec<_>>();

        assert!(matches!(events.as_slice(), [
            DeploymentLogFollowEvent::Available(source),
            DeploymentLogFollowEvent::Unavailable(unavailable),
        ] if source.name == "web" && unavailable.name == "web"));
        let diagnostic_events = diagnostic_events(&records);
        assert_eq!(diagnostic_events.len(), 1);
        assert!(
            diagnostic_events[0]
                .get("cause")
                .is_some_and(|cause| cause.contains("daemon rejected [REDACTED]"))
        );
        assert!(!format!("{diagnostic_events:?}").contains(secret));
    }

    #[test]
    fn expected_history_capability_absence_emits_no_backend_diagnostic() {
        let source = source("simulated");
        let (_, records, _) = capture_records(|| {
            aggregate_history(
                vec![RuntimeLogSourceHistory {
                    source,
                    result: Err(RuntimeBackendError::log_capability_unavailable()),
                }],
                200,
                &ProtectedLogValues::default(),
                &diagnostic_context("read_history"),
            )
        });

        assert_eq!(diagnostic_events(&records), Vec::new());
    }

    #[test]
    fn history_before_the_workload_exists_emits_no_backend_diagnostic() {
        let (history, records, _) = capture_records(|| {
            aggregate_history(
                vec![RuntimeLogSourceHistory {
                    source: source("web"),
                    result: Err(RuntimeBackendError::workload_not_created()),
                }],
                200,
                &ProtectedLogValues::default(),
                &diagnostic_context("read_history"),
            )
        });

        assert_eq!(diagnostic_events(&records), Vec::new());
        assert!(history.entries.is_empty());
    }

    #[tokio::test]
    async fn follow_before_the_workload_exists_marks_the_source_unavailable_without_diagnostic() {
        let (events, records, _) = capture_records_async(|| async {
            follow_sources(
                DeploymentLogStreams::fixed(vec![RuntimeLogSourceStream {
                    source: source("web"),
                    result: Err(RuntimeBackendError::workload_not_created()),
                }]),
                ProtectedLogValues::default(),
                CancellationToken::new(),
                diagnostic_context("follow"),
            )
            .collect::<Vec<_>>()
            .await
        })
        .await;

        assert!(matches!(
            events.as_slice(),
            [Ok(DeploymentLogFollowEvent::Unavailable(unavailable))] if unavailable.name == "web"
        ));
        assert_eq!(diagnostic_events(&records), Vec::new());
    }

    #[tokio::test]
    async fn a_source_starting_mid_follow_is_announced_then_followed() {
        let (late_sender, late) = tokio::sync::mpsc::channel(1);
        let cancellation = CancellationToken::new();
        let output = follow_sources(
            DeploymentLogStreams {
                sources: Vec::new(),
                late: Some(late),
                failure: DeploymentLogFollowFailure::default(),
            },
            ProtectedLogValues::new(["private".to_owned()]),
            cancellation.clone(),
            diagnostic_context("follow"),
        );
        tokio::pin!(output);
        let step = source("prepare:engine-build");
        late_sender
            .send(RuntimeLogSourceStream {
                source: step.clone(),
                result: Ok(Box::pin(futures_util::stream::iter([Ok(
                    RuntimeLogEvent::Entry(entry(step.clone(), 1, b"building private")),
                )]))),
            })
            .await
            .expect("late source");

        let available = output.next().await;
        let built = output.next().await;
        let ended = output.next().await;
        drop(late_sender);

        assert!(matches!(
            available,
            Some(Ok(DeploymentLogFollowEvent::Available(source))) if source == step
        ));
        assert!(matches!(
            built,
            Some(Ok(DeploymentLogFollowEvent::Entry(entry))) if entry.content == b"building [REDACTED]"
        ));
        assert!(matches!(
            ended,
            Some(Ok(DeploymentLogFollowEvent::Ended(source))) if source == step
        ));
        assert!(output.next().await.is_none(), "no source can start anymore");
    }

    #[tokio::test]
    async fn a_source_starting_again_follows_only_after_its_earlier_stream_ended() {
        let model = source("model");
        let (earlier_sender, earlier) = tokio::sync::mpsc::unbounded_channel();
        let (late_sender, late) = tokio::sync::mpsc::channel(1);
        let output = follow_sources(
            DeploymentLogStreams {
                sources: vec![RuntimeLogSourceStream {
                    source: model.clone(),
                    result: Ok(Box::pin(
                        tokio_stream::wrappers::UnboundedReceiverStream::new(earlier),
                    )),
                }],
                late: Some(late),
                failure: DeploymentLogFollowFailure::default(),
            },
            ProtectedLogValues::default(),
            CancellationToken::new(),
            diagnostic_context("follow"),
        );
        tokio::pin!(output);
        let available = output.next().await;

        late_sender
            .send(RuntimeLogSourceStream {
                source: model.clone(),
                result: Ok(Box::pin(futures_util::stream::iter([Ok(
                    RuntimeLogEvent::Entry(entry(model.clone(), 2, b"restarted")),
                )]))),
            })
            .await
            .expect("restarted source");
        // Lets the single-threaded test runtime run the announcement before the earlier stream
        // ends, so a forwarder that did not wait would overtake it.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        earlier_sender
            .send(Ok(RuntimeLogEvent::Entry(entry(
                model.clone(),
                1,
                b"first",
            ))))
            .expect("earlier stream");
        drop(earlier_sender);
        drop(late_sender);
        let rest = output.collect::<Vec<_>>().await;

        assert_eq!(
            std::iter::once(available.expect("first event"))
                .chain(rest)
                .map(|event| describe(&event.expect("fan-in event")))
                .collect::<Vec<_>>(),
            [
                "model available",
                "model entry first",
                "model ended",
                "model available",
                "model entry restarted",
                "model ended",
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_follow_ends_with_an_error_after_its_events_and_no_ended_source() {
        let model = source("model");
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let failure = DeploymentLogFollowFailure::default();
        let output = follow_sources(
            DeploymentLogStreams {
                sources: vec![RuntimeLogSourceStream {
                    source: model.clone(),
                    result: Ok(Box::pin(
                        tokio_stream::wrappers::UnboundedReceiverStream::new(receiver),
                    )),
                }],
                late: None,
                failure: failure.clone(),
            },
            ProtectedLogValues::default(),
            CancellationToken::new(),
            diagnostic_context("follow"),
        );

        sender
            .send(Ok(RuntimeLogEvent::Entry(entry(
                model.clone(),
                1,
                b"first",
            ))))
            .expect("source stream");
        failure.fail();
        drop(sender);
        let events = output.collect::<Vec<_>>().await;

        assert!(
            matches!(
                events.as_slice(),
                [
                    Ok(DeploymentLogFollowEvent::Available(available)),
                    Ok(DeploymentLogFollowEvent::Entry(first)),
                    Err(status),
                ] if available == &model
                    && first.content == b"first"
                    && status.code() == tonic::Code::Unavailable
            ),
            "{events:?}"
        );
    }

    #[test]
    fn real_history_failure_emits_one_filtered_bounded_diagnostic() {
        let source = source("web");
        let secret = "private-token";
        let error = RuntimeBackendError::docker_with_source(
            "reading Docker logs",
            std::io::Error::other(format!("daemon rejected {secret} {}", "🦀".repeat(20_000))),
        );
        let (_, records, _) = capture_records(|| {
            aggregate_history(
                vec![RuntimeLogSourceHistory {
                    source,
                    result: Err(error),
                }],
                200,
                &ProtectedLogValues::new([secret.to_owned()]),
                &diagnostic_context("read_history"),
            )
        });
        let events = diagnostic_events(&records);

        assert_eq!(events.len(), 1);
        let fields = &events[0];
        assert_eq!(fields.get("owner_kind"), Some(&"model".to_owned()));
        assert_eq!(
            fields.get("deployment_id"),
            Some(&"deployment-1".to_owned())
        );
        assert_eq!(fields.get("operation"), Some(&"read_history".to_owned()));
        assert_eq!(fields.get("source_name"), Some(&"web".to_owned()));
        assert!(
            fields
                .get("cause")
                .is_some_and(|cause| cause.contains("daemon rejected [REDACTED]"))
        );
        assert!(!format!("{fields:?}").contains(secret));
        assert!(
            fields
                .get("cause")
                .is_some_and(|cause| cause.len() <= MAX_LOG_DIAGNOSTIC_BYTES)
        );
    }

    #[test]
    fn wrapped_expected_request_outcome_emits_no_backend_diagnostic() {
        let error = DeploymentLogError::configuration(anyhow::Error::new(
            RuntimeBackendError::LogCancelled,
        ));
        let (_, records, _) = capture_records(|| {
            map_runtime_error_with_diagnostic(
                error,
                &diagnostic_context("follow"),
                Some(&ProtectedLogValues::default()),
            )
        });

        assert_eq!(diagnostic_events(&records), Vec::new());
    }

    #[test]
    fn real_request_failure_emits_one_filtered_diagnostic() {
        let secret = "private-token";
        let error = DeploymentLogError::configuration(anyhow::Error::new(
            RuntimeBackendError::docker_with_source(
                "opening Docker logs",
                std::io::Error::other(format!("daemon rejected {secret}")),
            ),
        ));
        let (_, records, _) = capture_records(|| {
            map_runtime_error_with_diagnostic(
                error,
                &diagnostic_context("follow"),
                Some(&ProtectedLogValues::new([secret.to_owned()])),
            )
        });
        let events = diagnostic_events(&records);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].get("source_name"), Some(&"unknown".to_owned()));
        assert!(
            events[0]
                .get("cause")
                .is_some_and(|cause| cause.contains("daemon rejected [REDACTED]"))
        );
        assert!(!format!("{events:?}").contains(secret));
        let cause = events[0].get("cause").expect("diagnostic cause");
        assert_eq!(cause.matches("opening Docker logs").count(), 1);
        assert_eq!(cause.matches("daemon rejected").count(), 1);
    }

    #[test]
    fn transport_status_mapping_never_exposes_owner_or_runtime_details() {
        for (error, code, message) in [
            (
                DeploymentLogError::InvalidSource,
                tonic::Code::InvalidArgument,
                "Deployment log request contains an invalid source",
            ),
            (
                DeploymentLogError::NotFound,
                tonic::Code::NotFound,
                "Deployment log deployment was not found",
            ),
            (
                DeploymentLogError::UnsupportedDeployment,
                tonic::Code::FailedPrecondition,
                "Deployment logs are not available for this deployment",
            ),
            (
                DeploymentLogError::NodeUnavailable,
                tonic::Code::Unavailable,
                "Deployment logs are unavailable right now; retry shortly",
            ),
            (
                DeploymentLogError::configuration(anyhow::anyhow!("runtime-native-id-123")),
                tonic::Code::FailedPrecondition,
                "Deployment logs are not available for this deployment",
            ),
        ] {
            let status = map_runtime_error(error);
            assert_eq!(status.code(), code);
            assert_eq!(status.message(), message);
            assert!(!status.message().contains("runtime-native-id-123"));
        }
        let status = map_admission_error(ManagementLogAdmissionError::Exhausted);
        assert_eq!(status.code(), tonic::Code::ResourceExhausted);
        assert_eq!(
            status.message(),
            "Deployment log request capacity is exhausted; retry shortly"
        );
    }

    #[test]
    fn cloned_admission_enforces_one_eight_request_budget_and_releases_permits() {
        let model = ManagementLogAdmission::new();
        let application = model.clone();
        let permits = (0..8)
            .map(|_| model.try_acquire().expect("shared permit"))
            .collect::<Vec<_>>();

        assert_eq!(
            application
                .try_acquire()
                .expect_err("shared budget exhausted"),
            ManagementLogAdmissionError::Exhausted
        );
        drop(permits);
        assert!(application.try_acquire().is_ok());
    }

    fn diagnostic_context(operation: &'static str) -> DeploymentLogDiagnosticContext {
        DeploymentLogDiagnosticContext::new("model", "deployment-1", operation)
    }

    fn diagnostic_events(records: &[Record]) -> Vec<BTreeMap<String, String>> {
        records
            .iter()
            .filter_map(|record| match record {
                Record::Event { level, fields } if level == "ERROR" => Some(fields.clone()),
                _ => None,
            })
            .collect()
    }

    fn source(name: &str) -> RuntimeLogSource {
        RuntimeLogSource {
            name: name.to_owned(),
            label: format!("{name} label"),
        }
    }

    /// One fan-in event as `<source> <kind> [content]`, for exact sequence assertions.
    fn describe(event: &DeploymentLogFollowEvent) -> String {
        match event {
            DeploymentLogFollowEvent::Available(source) => format!("{} available", source.name),
            DeploymentLogFollowEvent::Unavailable(source) => {
                format!("{} unavailable", source.name)
            }
            DeploymentLogFollowEvent::Ended(source) => format!("{} ended", source.name),
            DeploymentLogFollowEvent::Entry(entry) => format!(
                "{} entry {}",
                entry.source.name,
                String::from_utf8_lossy(&entry.content)
            ),
        }
    }

    fn entry(source: RuntimeLogSource, seconds: u64, content: &[u8]) -> RuntimeLogEntry {
        RuntimeLogEntry {
            source,
            timestamp: UNIX_EPOCH + Duration::from_secs(seconds),
            stream: RuntimeLogStreamKind::Stdout,
            content: content.to_vec(),
            truncated: false,
        }
    }

    fn history(
        source: RuntimeLogSource,
        entries: Vec<RuntimeLogEntry>,
        has_older_output: bool,
    ) -> RuntimeLogSourceHistory {
        RuntimeLogSourceHistory {
            source,
            result: Ok(RuntimeLogHistory {
                entries,
                has_older_output,
                candidate_budget_truncated: false,
            }),
        }
    }
}
