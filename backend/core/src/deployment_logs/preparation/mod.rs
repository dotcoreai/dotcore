//! Retains the output of the preparation steps a node runs for the deployments it hosts, such
//! as a model download or an engine build, and serves each step as a deployment log source.
//!
//! Every step of a deployment keeps a bounded log in the node store, readable by timestamp while
//! the step runs, after it finishes, and across backend restarts; a follow of a step ends once
//! the step stops writing, and the step is listed again when it starts again. Entries of one
//! step have strictly increasing timestamps, even across restarts and backward clock steps, so a
//! follow resumed after its latest timestamp neither repeats nor skips entries. Each step that
//! does work in a preparation attempt writes a stage line when it starts, waits and ends, so a
//! retried attempt appends after the failed one. A step that finds its artifact prepared writes
//! one line naming it, only when that differs from what the step last prepared or found, so
//! repeated preparations and restarts add nothing. A deployment's logs live until its realization
//! is removed from the node.

mod file;

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::Context as _;
use dotcore_artifact::{
    OutputStream, PreparationEvent, PreparationEventKind, PreparationOutput, PreparationStep,
};
use dotcore_domain::clock::Clock;
use dotcore_runtime::{
    MAX_RUNTIME_LOG_SOURCES, RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent,
    RuntimeLogFollowQuery, RuntimeLogHistory, RuntimeLogQuery, RuntimeLogSource,
    RuntimeLogSourceHistory, RuntimeLogSourceStream, RuntimeLogStream, RuntimeLogStreamKind,
};
use futures_util::StreamExt as _;
use tokio::sync::{broadcast, mpsc};
use tracing::warn;

use self::file::{Record, StepFileWriter, StepFiles};
use crate::management_logs::{
    DeploymentLogError, DeploymentLogFollowQuery, MAX_LOG_ENTRY_CONTENT_BYTES,
};

/// Directory of the preparation logs inside the node store.
const PREPARATION_LOG_DIRECTORY: &str = "preparation-logs";

/// Largest size of one step log file. A step retains at most twice this, its current and its
/// rotated file: room for tens of thousands of build lines, the tail of which explains a failure,
/// while a history read, which parses the whole retained log, stays quick.
const STEP_LOG_CAP_BYTES: u64 = 4 * 1024 * 1024;

/// Entries buffered per followed step before a slow follower re-reads the log instead.
const FOLLOW_BUFFER: usize = 1024;

/// Step starts buffered before a follow listing late steps scans the store again instead.
const STARTED_BUFFER: usize = 64;

/// Prefix of every step source name, which no workload source uses.
const STEP_SOURCE_PREFIX: &str = "prepare:";

/// Node-local preparation logs of the deployments a node hosts, kept in its node store.
///
/// Writers append one step's output as it runs; readers page and follow the same step by
/// timestamp. Clones share the same logs and live followers.
#[derive(Clone)]
pub(crate) struct PreparationLogs {
    store: Arc<Store>,
}

impl PreparationLogs {
    /// Opens the preparation logs kept in the node store at `node_store`, timing preparation
    /// attempts with `clock`; nothing is read or created until a step is used.
    pub(crate) fn new(node_store: &Path, clock: Arc<dyn Clock>) -> Self {
        Self::with_cap(node_store, clock, STEP_LOG_CAP_BYTES)
    }

    fn with_cap(node_store: &Path, clock: Arc<dyn Clock>, cap: u64) -> Self {
        Self {
            store: Arc::new(Store {
                root: node_store.join(PREPARATION_LOG_DIRECTORY),
                cap,
                clock,
                state: Mutex::new(StoreState::default()),
                started: Arc::new(Activations::new()),
            }),
        }
    }

    /// Returns the output of one preparation attempt of a deployment, which writes each step's
    /// events to that step's log.
    ///
    /// A step that does work writes a line saying it is preparing, or preparing again after an
    /// earlier attempt, then its tool's lines, any wait for a concurrent preparation, and how it
    /// ended. A step that finds its artifact prepared writes one line naming it, unless the step
    /// last finished preparing or found that same artifact. Once the deployment's logs are
    /// removed, the attempt writes nothing more, even for steps it starts afterwards.
    pub(crate) fn output(&self, owner: PreparationOwner, deployment_id: &str) -> PreparationOutput {
        let store = Arc::clone(&self.store);
        let deployment_id = deployment_id.to_owned();
        let removals = store.removals(owner, &deployment_id);
        // One writer per step for the whole attempt; `None` once the logs were removed.
        let writers = Mutex::new(BTreeMap::<PreparationStep, Option<Arc<StepLog>>>::new());
        PreparationOutput::new(
            Arc::clone(&self.store.clock),
            Arc::new(move |event: PreparationEvent| {
                let log = lock(&writers)
                    .entry(event.step.clone())
                    .or_insert_with(|| store.writer(&deployment_id, &event.step, removals))
                    .clone();
                if let Some(log) = log {
                    PreparationStepWriter { log }.record(event.time, event.kind);
                }
            }),
        )
    }

    /// Returns the writer of one step of a deployment.
    ///
    /// A writer obtained before the deployment's logs are removed drops everything written after
    /// the removal, so a cancelled attempt cannot bring removed logs back.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn writer(
        &self,
        deployment_id: &str,
        step: PreparationStep,
    ) -> PreparationStepWriter {
        PreparationStepWriter {
            log: self.store.step(deployment_id, &step),
        }
    }

    /// Splits requested source names into the deployment's step sources and its workload
    /// sources.
    ///
    /// With no requested name, every retained step among `known` is selected, in the order the
    /// steps started, keeping room for `workload_sources` within the source limit, and every
    /// workload source is selected too. Otherwise each name naming a step among `known` selects
    /// that step, whether or not it has started, and the other names are left to the owner.
    pub(crate) fn select(
        &self,
        owner: PreparationOwner,
        deployment_id: &str,
        requested: &[String],
        known: &[PreparationStep],
        workload_sources: usize,
    ) -> SourceSelection {
        if requested.is_empty() {
            let mut steps = self.store.started_steps(owner, deployment_id, known);
            steps.truncate(MAX_RUNTIME_LOG_SOURCES.saturating_sub(workload_sources));
            return SourceSelection {
                steps,
                workload: Some(Vec::new()),
            };
        }
        let (steps, workload): (Vec<_>, Vec<_>) = requested
            .iter()
            .map(|name| match step_from_source_name(name) {
                Some(step) if known.contains(&step) => Ok(step),
                _ => Err(name.clone()),
            })
            .partition(Result::is_ok);
        SourceSelection {
            steps: steps.into_iter().filter_map(Result::ok).collect(),
            workload: (!workload.is_empty())
                .then(|| workload.into_iter().filter_map(Result::err).collect()),
        }
    }

    /// Reads one bounded history per step, newest entries strictly before the query's optional
    /// timestamp.
    pub(crate) async fn read(
        &self,
        deployment_id: &str,
        steps: &[PreparationStep],
        query: &RuntimeLogQuery,
    ) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
        let mut histories = Vec::with_capacity(steps.len());
        for step in steps {
            let log = self.store.step(deployment_id, step);
            let records = log
                .read()
                .await
                .with_context(|| format!("read the {} preparation log", step_label(step)))
                .map_err(DeploymentLogError::configuration)?;
            histories.push(RuntimeLogSourceHistory {
                source: step_source(step),
                result: Ok(history_page(&step_source(step), records, query)),
            });
        }
        Ok(histories)
    }

    /// Follows each step after its resume position until `cancellation` fires. A follow ends
    /// once its step is idle and everything it wrote was delivered, at once for a step already
    /// done, and ends as unavailable once the deployment's logs are removed.
    pub(crate) fn follow(
        &self,
        deployment_id: &str,
        steps: &[PreparationStep],
        resume_positions: &BTreeMap<String, SystemTime>,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> Vec<RuntimeLogSourceStream> {
        steps
            .iter()
            .map(|step| {
                let source = step_source(step);
                let query = RuntimeLogFollowQuery::new(
                    resume_positions.get(&source.name).copied(),
                    cancellation.clone(),
                );
                RuntimeLogSourceStream {
                    result: Ok(self
                        .store
                        .step(deployment_id, step)
                        .follow(source.clone(), query)),
                    source,
                }
            })
            .collect()
    }

    /// Follows each step among `known` that starts after the follow opened with `opened` and
    /// `workload_sources` workload sources, as a new source once the step writes its first entry,
    /// and lists a step again each time it becomes active again, as in a new attempt; steps that
    /// never start are never listed. A step listed for the first time past the source limit is
    /// listed as unavailable instead. The receiver closes once the query's cancellation fires.
    pub(crate) fn follow_started(
        &self,
        owner: PreparationOwner,
        deployment_id: &str,
        known: Vec<PreparationStep>,
        opened: &[PreparationStep],
        workload_sources: usize,
        query: &DeploymentLogFollowQuery,
    ) -> mpsc::Receiver<RuntimeLogSourceStream> {
        let (sender, receiver) = mpsc::channel(known.len().max(1));
        let logs = self.clone();
        let deployment_id = deployment_id.to_owned();
        let mut listed = opened.iter().cloned().collect::<BTreeSet<_>>();
        let mut room = MAX_RUNTIME_LOG_SOURCES.saturating_sub(workload_sources + opened.len());
        let resume_positions = query.resume_positions().clone();
        let cancellation = query.cancellation().clone();
        // Subscribing before the first scan means no start falls between the two.
        let mut started = self.store.started.sender.subscribe();
        tokio::spawn(async move {
            // A step listed again takes no more room than its first listing did.
            let mut follow = |step: &PreparationStep, first: bool| {
                let source = step_source(step);
                if first && room == 0 {
                    return RuntimeLogSourceStream {
                        source,
                        result: Err(RuntimeBackendError::LogSourceLimitReached),
                    };
                }
                if first {
                    room -= 1;
                }
                let query = RuntimeLogFollowQuery::new(
                    resume_positions.get(&source.name).copied(),
                    cancellation.clone(),
                );
                RuntimeLogSourceStream {
                    result: Ok(logs
                        .store
                        .step(&deployment_id, step)
                        .follow(source.clone(), query)),
                    source,
                }
            };
            // Steps a scan listed first, with the number of activations announced when it listed
            // them: their follows hold those activations, so their announcements are not listed
            // again.
            let mut scanned = BTreeMap::<PreparationStep, u64>::new();
            let mut scan = true;
            let mut rescan = false;
            loop {
                if scan {
                    scan = false;
                    let store = Arc::clone(&logs.store);
                    let (id, candidates) = (deployment_id.clone(), known.clone());
                    let Ok(steps) = tokio::task::spawn_blocking(move || {
                        store.started_steps(owner, &id, &candidates)
                    })
                    .await
                    else {
                        return;
                    };
                    // Each follow listed below starts after this, so it holds every activation
                    // announced so far.
                    let announced = logs.store.started.announced();
                    for step in steps {
                        let first = listed.insert(step.clone());
                        if first {
                            scanned.insert(step.clone(), announced);
                        } else if !rescan {
                            continue;
                        }
                        // After missed announcements, a step listed earlier may have started
                        // again, so it is listed again too.
                        if sender.send(follow(&step, first)).await.is_err() {
                            return;
                        }
                    }
                    continue;
                }
                let event = tokio::select! {
                    () = cancellation.cancelled() => return,
                    () = sender.closed() => return,
                    event = started.recv() => event,
                };
                match event {
                    Ok(Activation {
                        deployment_id: id,
                        step,
                        number,
                    }) if id == deployment_id && known.contains(&step) => {
                        if scanned.get(&step).is_some_and(|scanned| number < *scanned) {
                            continue;
                        }
                        let first = listed.insert(step.clone());
                        if sender.send(follow(&step, first)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        scan = true;
                        rescan = true;
                    }
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
        receiver
    }

    /// Removes every step log of one deployment, ending its follows. Writers and preparation
    /// attempts obtained earlier write nothing more.
    pub(crate) fn remove(&self, owner: PreparationOwner, deployment_id: &str) -> io::Result<()> {
        self.store.remove(owner, deployment_id)
    }

    /// Opens the preparation logs kept in the node store at `node_store`, timed by the system
    /// clock.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn on_system_clock(node_store: &Path) -> Self {
        Self::new(node_store, Arc::new(dotcore_domain::clock::SystemClock))
    }
}

/// The sources one log request selects: preparation steps, and the workload sources the owner
/// resolves itself.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct SourceSelection {
    /// Selected steps, in the order their sources are returned.
    pub(crate) steps: Vec<PreparationStep>,

    /// Requested workload source names, empty when every workload source is selected, or `None`
    /// when only steps are.
    pub(crate) workload: Option<Vec<String>>,
}

/// Owner kind of the deployments whose steps a log holds; each keeps its own directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum PreparationOwner {
    Model,

    Application,
}

impl PreparationOwner {
    fn directory(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Application => "application",
        }
    }
}

/// The steps a model deployment may run.
pub(crate) fn model_steps() -> [PreparationStep; 3] {
    [
        PreparationStep::ModelDownload,
        PreparationStep::EngineBuild,
        PreparationStep::EngineImage,
    ]
}

/// The steps one application service may run.
pub(crate) fn service_steps(service: &str) -> [PreparationStep; 2] {
    [
        PreparationStep::ServiceBuild {
            service: service.to_owned(),
        },
        PreparationStep::ServiceImage {
            service: service.to_owned(),
        },
    ]
}

/// Returns a step's public log source.
pub(crate) fn step_source(step: &PreparationStep) -> RuntimeLogSource {
    RuntimeLogSource {
        name: format!("{STEP_SOURCE_PREFIX}{}", step_key(step, ':')),
        label: step_label(step),
    }
}

/// Parses a step source name; `None` for any other source.
pub(crate) fn step_from_source_name(name: &str) -> Option<PreparationStep> {
    step_from_key(name.strip_prefix(STEP_SOURCE_PREFIX)?, ':')
}

fn step_owner(step: &PreparationStep) -> PreparationOwner {
    match step {
        PreparationStep::ModelDownload
        | PreparationStep::EngineBuild
        | PreparationStep::EngineImage => PreparationOwner::Model,
        PreparationStep::ServiceBuild { .. } | PreparationStep::ServiceImage { .. } => {
            PreparationOwner::Application
        }
    }
}

fn step_label(step: &PreparationStep) -> String {
    match step {
        PreparationStep::ModelDownload => "Model download".to_owned(),
        PreparationStep::EngineBuild => "Engine build".to_owned(),
        PreparationStep::EngineImage => "Engine image".to_owned(),
        PreparationStep::ServiceBuild { service } => format!("{service} build"),
        PreparationStep::ServiceImage { service } => format!("{service} image"),
    }
}

/// Returns a step's identity, with `separator` between a service and its step; service names
/// never hold `:` or `.`, so either separator is unambiguous.
fn step_key(step: &PreparationStep, separator: char) -> String {
    match step {
        PreparationStep::ModelDownload => "download".to_owned(),
        PreparationStep::EngineBuild => "engine-build".to_owned(),
        PreparationStep::EngineImage => "engine-image".to_owned(),
        PreparationStep::ServiceBuild { service } => format!("{service}{separator}build"),
        PreparationStep::ServiceImage { service } => format!("{service}{separator}image"),
    }
}

fn step_from_key(key: &str, separator: char) -> Option<PreparationStep> {
    match key {
        "download" => Some(PreparationStep::ModelDownload),
        "engine-build" => Some(PreparationStep::EngineBuild),
        "engine-image" => Some(PreparationStep::EngineImage),
        _ => {
            let (service, step) = key.rsplit_once(separator)?;
            if !dotcore_domain::valid_id(service) {
                return None;
            }
            let service = service.to_owned();
            match step {
                "build" => Some(PreparationStep::ServiceBuild { service }),
                "image" => Some(PreparationStep::ServiceImage { service }),
                _ => None,
            }
        }
    }
}

/// Returns a step's file name stem. A service name that is not a plain identifier is
/// hex-encoded, as deployment directories are, so it never names a path outside its directory.
fn step_file_stem(step: &PreparationStep) -> String {
    match step {
        PreparationStep::ServiceBuild { service } | PreparationStep::ServiceImage { service }
            if !dotcore_domain::valid_id(service) =>
        {
            let step = step_key(step, '.');
            let kind = step.rsplit_once('.').map_or("", |(_, kind)| kind);
            format!("{}.{kind}", path_safe(service))
        }
        _ => step_key(step, '.'),
    }
}

/// Returns `name` when it is a plain identifier, else its bytes hex-encoded behind a prefix no
/// identifier uses.
fn path_safe(name: &str) -> String {
    if dotcore_domain::valid_id(name) {
        name.to_owned()
    } else {
        let hex = name
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("_{hex}")
    }
}

/// Appends the output of one deployment step to its log.
///
/// Writing never fails for the caller: a log that cannot be written records one warning and
/// drops the rest of the step's output.
#[derive(Clone)]
pub(crate) struct PreparationStepWriter {
    log: Arc<StepLog>,
}

impl PreparationStepWriter {
    /// Appends one line of output read at `time` from `stream`.
    ///
    /// Each newline-separated part becomes its own entry, and a part longer than the entry limit
    /// is split. An entry takes `time` unless that would not follow the step's latest entry, in
    /// which case it takes the next nanosecond after it.
    pub(crate) fn append(&self, time: SystemTime, stream: RuntimeLogStreamKind, line: &[u8]) {
        self.log.append(time, stream, line);
    }

    /// Writes one event a preparation step reported at `time`: its tool's lines on their stream,
    /// and its stages and decisions as stdout lines.
    fn record(&self, time: SystemTime, kind: PreparationEventKind) {
        let stage = |line: &str| self.append(time, RuntimeLogStreamKind::Stdout, line.as_bytes());
        match kind {
            PreparationEventKind::Started { artifact } => self.log.start_attempt(time, artifact),
            PreparationEventKind::AlreadyPrepared { artifact } => {
                self.log.already_prepared(time, &artifact);
            }
            PreparationEventKind::Waiting => stage("Waiting for a concurrent preparation"),
            PreparationEventKind::Line { stream, text } => {
                let stream = match stream {
                    OutputStream::Stdout => RuntimeLogStreamKind::Stdout,
                    OutputStream::Stderr => RuntimeLogStreamKind::Stderr,
                };
                self.append(time, stream, text.as_bytes());
            }
            PreparationEventKind::Finished { duration } => self
                .log
                .finish_attempt(time, &format!("Finished in {}", display_duration(duration))),
            PreparationEventKind::Failed { error } => {
                stage(&format!("Failed: {error}"));
                self.log.end_activity();
            }
        }
    }
}

/// The steps of every deployment, shared by writers, readers and followers.
struct Store {
    root: PathBuf,
    cap: u64,
    clock: Arc<dyn Clock>,
    state: Mutex<StoreState>,

    /// Announces each step that becomes active, to follows listing late steps.
    started: Arc<Activations>,
}

/// Announces each step that becomes active, numbered in order, so a follow listing late steps
/// can tell which ones a scan of the store already saw.
struct Activations {
    sender: broadcast::Sender<Activation>,
    count: AtomicU64,
}

impl Activations {
    fn new() -> Self {
        Self {
            sender: broadcast::channel(STARTED_BUFFER).0,
            count: AtomicU64::new(0),
        }
    }

    /// Announces that a step became active, after its first record of the activity was written.
    fn announce(&self, deployment_id: &str, step: &PreparationStep) {
        let number = self.count.fetch_add(1, Ordering::SeqCst);
        // No follow listing late steps is a normal outcome.
        let _ = self.sender.send(Activation {
            deployment_id: deployment_id.to_owned(),
            step: step.clone(),
            number,
        });
    }

    /// How many activations were announced so far.
    fn announced(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }
}

/// One step that became active.
#[derive(Clone, Debug)]
struct Activation {
    deployment_id: String,
    step: PreparationStep,
    number: u64,
}

/// The steps in use and the removals of every deployment, guarded together so a writer is never
/// created for logs removed since its attempt began.
#[derive(Default)]
struct StoreState {
    /// Steps a writer, reader or follower holds, keyed by deployment and step.
    steps: BTreeMap<(String, PreparationStep), Arc<StepLog>>,

    /// How many times each deployment's logs were removed.
    removals: BTreeMap<(PreparationOwner, String), u64>,
}

impl Store {
    /// Returns the shared log of one step, adding it for this use.
    fn step(&self, deployment_id: &str, step: &PreparationStep) -> Arc<StepLog> {
        let mut state = lock(&self.state);
        self.step_locked(&mut state, deployment_id, step)
    }

    /// Returns the log of one step for a preparation attempt begun after `removals` removals of
    /// the deployment's logs, or `None` when they were removed since.
    fn writer(
        &self,
        deployment_id: &str,
        step: &PreparationStep,
        removals: u64,
    ) -> Option<Arc<StepLog>> {
        let mut state = lock(&self.state);
        let key = (step_owner(step), deployment_id.to_owned());
        if state.removals.get(&key).copied().unwrap_or_default() != removals {
            return None;
        }
        Some(self.step_locked(&mut state, deployment_id, step))
    }

    fn removals(&self, owner: PreparationOwner, deployment_id: &str) -> u64 {
        lock(&self.state)
            .removals
            .get(&(owner, deployment_id.to_owned()))
            .copied()
            .unwrap_or_default()
    }

    fn step_locked(
        &self,
        state: &mut StoreState,
        deployment_id: &str,
        step: &PreparationStep,
    ) -> Arc<StepLog> {
        let key = (deployment_id.to_owned(), step.clone());
        if let Some(log) = state.steps.get(&key) {
            return Arc::clone(log);
        }
        // Steps no one holds anymore keep nothing their files do not, so they are dropped here.
        state.steps.retain(|_, log| Arc::strong_count(log) > 1);
        let directory = self.deployment_directory(step_owner(step), deployment_id);
        let log = Arc::new(StepLog::new(
            StepFiles::new(&directory, &step_file_stem(step)),
            self.cap,
            deployment_id,
            step.clone(),
            Arc::clone(&self.started),
        ));
        state.steps.insert(key, Arc::clone(&log));
        log
    }

    /// Returns the steps among `known` that retain output, in the order they started.
    fn started_steps(
        &self,
        owner: PreparationOwner,
        deployment_id: &str,
        known: &[PreparationStep],
    ) -> Vec<PreparationStep> {
        let directory = self.deployment_directory(owner, deployment_id);
        let mut started = known
            .iter()
            .filter_map(|step| {
                let files = StepFiles::new(&directory, &step_file_stem(step));
                files
                    .exists()
                    .then(|| (files.first_time_ns().unwrap_or_default(), step.clone()))
            })
            .collect::<Vec<_>>();
        started.sort();
        started.into_iter().map(|(_, step)| step).collect()
    }

    fn remove(&self, owner: PreparationOwner, deployment_id: &str) -> io::Result<()> {
        let retired = {
            let mut state = lock(&self.state);
            *state
                .removals
                .entry((owner, deployment_id.to_owned()))
                .or_default() += 1;
            let keys = state
                .steps
                .keys()
                .filter(|(id, step)| id == deployment_id && step_owner(step) == owner)
                .cloned()
                .collect::<Vec<_>>();
            keys.into_iter()
                .filter_map(|key| state.steps.remove(&key))
                .collect::<Vec<_>>()
        };
        // Retired logs and the bumped removal count keep earlier attempts from writing again, so
        // the directory is removed without holding the store.
        for log in retired {
            log.retire();
        }
        match std::fs::remove_dir_all(self.deployment_directory(owner, deployment_id)) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }

    /// Returns the directory of one deployment's steps, which never names a path outside the
    /// store.
    fn deployment_directory(&self, owner: PreparationOwner, deployment_id: &str) -> PathBuf {
        self.root
            .join(owner.directory())
            .join(path_safe(deployment_id))
    }
}

/// One deployment step's log: its files, its writer state, and its live followers.
struct StepLog {
    files: StepFiles,
    cap: u64,
    deployment_id: String,
    step: PreparationStep,
    state: Mutex<StepState>,
    events: broadcast::Sender<StepEvent>,
    started: Arc<Activations>,
}

/// Writer state of one step, guarded so records are written and announced in time order.
#[derive(Default)]
struct StepState {
    /// Open file writer, from the first append on.
    writer: Option<StepFileWriter>,

    /// Time of the step's latest record, read from its files on first use.
    latest_time_ns: Option<u64>,

    /// Whether the deployment's logs were removed, which ends this log.
    retired: bool,

    /// Whether a write failed, which drops the rest of the step's output.
    failed: bool,

    /// Artifact the step's current attempt prepares, recorded as its decision once it finishes.
    attempt: Option<String>,

    /// Whether the step is writing in this process: from a record written while it was idle
    /// until its attempt finishes, fails or finds its artifact prepared. A step starts idle, so
    /// output retained from before a restart is complete.
    active: bool,
}

/// What a step announces to its followers.
#[derive(Clone, Debug)]
enum StepEvent {
    Record(Record),

    /// The step stopped writing; the records before this event are all it wrote.
    Idle,

    Retired,
}

impl StepLog {
    fn new(
        files: StepFiles,
        cap: u64,
        deployment_id: &str,
        step: PreparationStep,
        started: Arc<Activations>,
    ) -> Self {
        Self {
            files,
            cap,
            deployment_id: deployment_id.to_owned(),
            step,
            state: Mutex::new(StepState::default()),
            events: broadcast::channel(FOLLOW_BUFFER).0,
            started,
        }
    }

    fn append(&self, time: SystemTime, stream: RuntimeLogStreamKind, line: &[u8]) {
        self.append_locked(&mut lock(&self.state), time, stream, line);
    }

    fn append_locked(
        &self,
        state: &mut StepState,
        time: SystemTime,
        stream: RuntimeLogStreamKind,
        line: &[u8],
    ) {
        if state.retired || state.failed {
            return;
        }
        if let Err(error) = self.try_append(state, time_ns(time), stream, line) {
            state.failed = true;
            warn!(
                event_kind = "preparation_log_write_failed",
                deployment_id = %self.deployment_id,
                step = %step_key(&self.step, ':'),
                error = %error,
                "Preparation log write failed; the rest of the step's output is dropped"
            );
        }
    }

    /// Writes the line that opens a preparation attempt of `artifact`: a first attempt, or a new
    /// one after output the step retains from earlier attempts. The step's last decision is
    /// forgotten until the attempt finishes, so a failed attempt leaves none.
    fn start_attempt(&self, time: SystemTime, artifact: String) {
        let mut state = lock(&self.state);
        let line: &[u8] = if state.latest_time_ns.is_some() || self.files.exists() {
            b"Preparing again in a new attempt"
        } else {
            b"Preparing"
        };
        self.append_locked(&mut state, time, RuntimeLogStreamKind::Stdout, line);
        self.record_decision(&mut state, None);
        state.attempt = Some(artifact);
    }

    /// Writes the line that ends a successful attempt and records its artifact as the step's
    /// decision.
    fn finish_attempt(&self, time: SystemTime, line: &str) {
        let mut state = lock(&self.state);
        self.append_locked(
            &mut state,
            time,
            RuntimeLogStreamKind::Stdout,
            line.as_bytes(),
        );
        if let Some(artifact) = state.attempt.take() {
            self.record_decision(&mut state, Some(&artifact));
        }
        Self::settle(&self.events, &mut state);
    }

    /// Marks the step idle once its preparation ended without finishing, as after a failure.
    fn end_activity(&self) {
        Self::settle(&self.events, &mut lock(&self.state));
    }

    /// Marks an active step idle and tells its followers, which then end.
    fn settle(events: &broadcast::Sender<StepEvent>, state: &mut StepState) {
        if std::mem::take(&mut state.active) {
            // No follower is a normal outcome.
            let _ = events.send(StepEvent::Idle);
        }
    }

    /// Writes one line naming the prepared `artifact` the step found, unless it is already the
    /// step's decision, and records it as that decision.
    fn already_prepared(&self, time: SystemTime, artifact: &str) {
        let mut state = lock(&self.state);
        if state.retired || state.failed {
            return;
        }
        // An unreadable decision only costs a repeated line.
        if self
            .files
            .decision()
            .is_ok_and(|decision| decision.as_deref() == Some(artifact))
        {
            return;
        }
        let line = already_prepared_line(&self.step, artifact);
        self.append_locked(
            &mut state,
            time,
            RuntimeLogStreamKind::Stdout,
            line.as_bytes(),
        );
        self.record_decision(&mut state, Some(artifact));
        Self::settle(&self.events, &mut state);
    }

    /// Records the step's decision, or forgets it with `None`, unless the step's log is retired
    /// or failed. A record that cannot be written only costs a repeated line, so it is logged and
    /// left.
    fn record_decision(&self, state: &mut StepState, artifact: Option<&str>) {
        if state.retired || state.failed {
            return;
        }
        if let Err(error) = self.files.record_decision(artifact) {
            warn!(
                event_kind = "preparation_decision_write_failed",
                deployment_id = %self.deployment_id,
                step = %step_key(&self.step, ':'),
                error = %error,
                "Preparation step decision could not be recorded; the step may repeat its decision line"
            );
        }
    }

    fn try_append(
        &self,
        state: &mut StepState,
        time_ns: u64,
        stream: RuntimeLogStreamKind,
        line: &[u8],
    ) -> io::Result<()> {
        let mut latest = match state.latest_time_ns {
            Some(latest) => Some(latest),
            None => self.files.last_time_ns()?,
        };
        let starts = !state.active;
        if state.writer.is_none() {
            state.writer = Some(StepFileWriter::open(self.files.clone(), self.cap)?);
        }
        let writer = state.writer.as_mut().expect("writer opened above");
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        for part in line.split(|byte| *byte == b'\n') {
            for content in chunks(part) {
                let record = Record {
                    time_ns: latest.map_or(time_ns, |latest| time_ns.max(latest + 1)),
                    stream,
                    content: content.to_vec(),
                };
                writer.append(&record)?;
                latest = Some(record.time_ns);
                state.latest_time_ns = latest;
                // No follower is a normal outcome.
                let _ = self.events.send(StepEvent::Record(record));
            }
        }
        if starts {
            state.active = true;
            self.started.announce(&self.deployment_id, &self.step);
        }
        Ok(())
    }

    /// Reads every retained record, oldest first, with a leading note when older output was
    /// dropped.
    async fn read(self: &Arc<Self>) -> io::Result<Vec<Record>> {
        let log = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let read = log.files.read()?;
            let mut records = read.records;
            if read.dropped
                && let Some(first) = records.first()
            {
                records.insert(0, dropped_note(first.time_ns.saturating_sub(1)));
            }
            Ok(records)
        })
        .await
        .map_err(io::Error::other)?
    }

    fn follow(
        self: Arc<Self>,
        source: RuntimeLogSource,
        query: RuntimeLogFollowQuery,
    ) -> RuntimeLogStream {
        // Subscribing before the first read means no record falls between the two, and checking
        // retirement and activity after subscribing means a removal or an end is either seen here
        // or received.
        let events = self.events.subscribe();
        let (retired, idle) = {
            let state = lock(&self.state);
            (state.retired, self.idle(&state))
        };
        let follow = Follow {
            events,
            log: self,
            source,
            latest_time_ns: query.since_timestamp().map(time_ns),
            pending: VecDeque::new(),
            reread: true,
            retired,
            idle,
            done: false,
        };
        let cancellation = query.cancellation().clone();
        Box::pin(
            futures_util::stream::unfold(follow, Follow::next)
                .take_until(cancellation.cancelled_owned()),
        )
    }

    /// Whether the step is done writing: it is not active and wrote output earlier. A step that
    /// never wrote anything has yet to start.
    fn idle(&self, state: &StepState) -> bool {
        !state.active && (state.latest_time_ns.is_some() || self.files.exists())
    }

    fn retire(&self) {
        let mut state = lock(&self.state);
        state.retired = true;
        state.writer = None;
        let _ = self.events.send(StepEvent::Retired);
    }
}

/// Position of one follow over a step's records.
struct Follow {
    log: Arc<StepLog>,
    events: broadcast::Receiver<StepEvent>,
    source: RuntimeLogSource,

    /// Time of the latest record returned, or of the resume position.
    latest_time_ns: Option<u64>,
    pending: VecDeque<Record>,

    /// Whether the log must be read again, at the start or after the follower fell behind.
    reread: bool,

    /// Whether the deployment's logs were removed before the follow subscribed.
    retired: bool,

    /// Whether the step stopped writing, so the follow ends once it delivered what was written.
    idle: bool,
    done: bool,
}

impl Follow {
    async fn next(mut self) -> Option<(Result<RuntimeLogEvent, RuntimeBackendError>, Self)> {
        if self.done {
            return None;
        }
        if self.retired {
            return Some(self.unavailable());
        }
        loop {
            if let Some(record) = self.pending.pop_front() {
                // Strictly increasing times make the latest time a complete resume position.
                if self
                    .latest_time_ns
                    .is_some_and(|latest| record.time_ns <= latest)
                {
                    continue;
                }
                self.latest_time_ns = Some(record.time_ns);
                let entry = log_entry(&self.source, record);
                return Some((Ok(RuntimeLogEvent::Entry(entry)), self));
            }
            if self.reread {
                self.reread = false;
                match self.log.read().await {
                    Ok(records) => self.pending.extend(records),
                    Err(error) => {
                        warn!(
                            event_kind = "preparation_log_follow_failed",
                            deployment_id = %self.log.deployment_id,
                            step = %step_key(&self.log.step, ':'),
                            error = %error,
                            "Preparation log follow could not read the step's log"
                        );
                        return Some(self.unavailable());
                    }
                }
                continue;
            }
            // Output the step writes after it went idle belongs to a new activity, which is
            // announced and followed again.
            if self.idle {
                return None;
            }
            match self.events.recv().await {
                Ok(StepEvent::Record(record)) => self.pending.push_back(record),
                Ok(StepEvent::Idle) => self.idle = true,
                Ok(StepEvent::Retired) | Err(broadcast::error::RecvError::Closed) => {
                    return Some(self.unavailable());
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // A missed end is read from the step itself; the reread then holds every
                    // record written before it.
                    self.idle = self.log.idle(&lock(&self.log.state));
                    self.reread = true;
                }
            }
        }
    }

    fn unavailable(mut self) -> (Result<RuntimeLogEvent, RuntimeBackendError>, Self) {
        self.done = true;
        let source = self.source.clone();
        (Ok(RuntimeLogEvent::SourceUnavailable(source)), self)
    }
}

/// Returns the line a step writes when it finds `artifact` already prepared.
fn already_prepared_line(step: &PreparationStep, artifact: &str) -> String {
    let decision = match step {
        PreparationStep::ModelDownload => "Already downloaded",
        PreparationStep::EngineBuild | PreparationStep::ServiceBuild { .. } => "Already built",
        PreparationStep::EngineImage | PreparationStep::ServiceImage { .. } => "Already present",
    };
    format!("{decision}: {artifact}")
}

/// Returns the newest entries strictly before the query's optional timestamp.
fn history_page(
    source: &RuntimeLogSource,
    mut records: Vec<Record>,
    query: &RuntimeLogQuery,
) -> RuntimeLogHistory {
    if let Some(before) = query.before_timestamp().map(time_ns) {
        records.retain(|record| record.time_ns < before);
    }
    let maximum = usize::from(query.maximum_entries().get());
    let has_older_output = records.len() > maximum;
    let entries = records
        .into_iter()
        .rev()
        .take(maximum)
        .rev()
        .map(|record| log_entry(source, record))
        .collect();
    RuntimeLogHistory {
        entries,
        has_older_output,
        candidate_budget_truncated: false,
    }
}

/// The entry standing for output a rotation dropped, just before the oldest retained one.
fn dropped_note(time_ns: u64) -> Record {
    Record {
        time_ns,
        stream: RuntimeLogStreamKind::Stdout,
        content: format!(
            "Earlier output of this step was dropped to keep its log within {} MiB.",
            STEP_LOG_CAP_BYTES * 2 / (1024 * 1024)
        )
        .into_bytes(),
    }
}

// Records are already split at the entry limit, so no entry is truncated here.
fn log_entry(source: &RuntimeLogSource, record: Record) -> RuntimeLogEntry {
    RuntimeLogEntry {
        source: source.clone(),
        timestamp: UNIX_EPOCH + Duration::from_nanos(record.time_ns),
        stream: record.stream,
        content: record.content,
        truncated: false,
    }
}

/// Renders a step duration to the tenth of a second under a minute, then to the second.
fn display_duration(duration: Duration) -> String {
    if duration < Duration::from_secs(60) {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        humantime::format_duration(Duration::from_secs(duration.as_secs())).to_string()
    }
}

/// Splits one line into parts within the entry limit; an empty line stays one empty part.
fn chunks(line: &[u8]) -> impl Iterator<Item = &[u8]> {
    let parts = line.chunks(MAX_LOG_ENTRY_CONTENT_BYTES);
    let empty = line.is_empty().then_some(&line[..0]);
    empty.into_iter().chain(parts)
}

fn time_ns(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).map_or(0, |duration| {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    })
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU16;

    use dotcore_domain::clock::SystemClock;
    use tokio_util::sync::CancellationToken;

    use super::*;

    const DEPLOYMENT: &str = "model-one";

    const STORE_PATH: &str = "/nix/store/00000000000000000000000000000001-engine";

    #[tokio::test]
    async fn history_pages_read_the_newest_entries_before_a_timestamp() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        for second in 1..=5 {
            writer.append(
                at(second),
                RuntimeLogStreamKind::Stderr,
                format!("line {second}").as_bytes(),
            );
        }

        let newest = read(&logs, PreparationStep::EngineBuild, 2, None).await;
        let older = read(&logs, PreparationStep::EngineBuild, 2, Some(at(4))).await;
        let oldest = read(&logs, PreparationStep::EngineBuild, 2, Some(at(2))).await;

        assert_eq!(contents(&newest), ["line 4", "line 5"]);
        assert!(newest.has_older_output);
        assert_eq!(contents(&older), ["line 2", "line 3"]);
        assert!(older.has_older_output);
        assert_eq!(contents(&oldest), ["line 1"]);
        assert!(!oldest.has_older_output);
        assert_eq!(
            newest
                .entries
                .iter()
                .map(|entry| (entry.timestamp, entry.stream))
                .collect::<Vec<_>>(),
            [
                (at(4), RuntimeLogStreamKind::Stderr),
                (at(5), RuntimeLogStreamKind::Stderr)
            ]
        );
        assert_eq!(newest.entries[0].source.name, "prepare:engine-build");
        assert_eq!(newest.entries[0].source.label, "Engine build");
    }

    #[tokio::test]
    async fn step_events_render_as_stage_lines_and_tool_lines_per_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let first = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        let events = [
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
            PreparationEventKind::Waiting,
            PreparationEventKind::Line {
                stream: OutputStream::Stderr,
                text: "building".to_owned(),
            },
            PreparationEventKind::Failed {
                error: "nix build failed".to_owned(),
            },
        ];
        for (second, kind) in (1..).zip(events) {
            first.record(at(second), kind);
        }

        // A later attempt, as from a restarted process, says it prepares again.
        let retried = PreparationLogs::on_system_clock(directory.path())
            .writer(DEPLOYMENT, PreparationStep::EngineBuild);
        retried.record(
            at(10),
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
        );
        retried.record(
            at(11),
            PreparationEventKind::Finished {
                duration: Duration::from_millis(3_400),
            },
        );
        retried.record(
            at(12),
            PreparationEventKind::Finished {
                duration: Duration::from_millis(75_400),
            },
        );

        let history = read(&logs, PreparationStep::EngineBuild, 20, None).await;
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| (
                    entry.stream,
                    String::from_utf8(entry.content.clone()).unwrap()
                ))
                .collect::<Vec<_>>(),
            [
                (RuntimeLogStreamKind::Stdout, "Preparing".to_owned()),
                (
                    RuntimeLogStreamKind::Stdout,
                    "Waiting for a concurrent preparation".to_owned()
                ),
                (RuntimeLogStreamKind::Stderr, "building".to_owned()),
                (
                    RuntimeLogStreamKind::Stdout,
                    "Failed: nix build failed".to_owned()
                ),
                (
                    RuntimeLogStreamKind::Stdout,
                    "Preparing again in a new attempt".to_owned()
                ),
                (RuntimeLogStreamKind::Stdout, "Finished in 3.4s".to_owned()),
                (
                    RuntimeLogStreamKind::Stdout,
                    "Finished in 1m 15s".to_owned()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn a_step_writes_each_new_decision_once_across_passes_restarts_and_attempts() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        let found = |artifact: &str| PreparationEventKind::AlreadyPrepared {
            artifact: artifact.to_owned(),
        };
        let started = |artifact: &str| PreparationEventKind::Started {
            artifact: artifact.to_owned(),
        };
        let finished = PreparationEventKind::Finished {
            duration: Duration::from_secs(2),
        };
        let failed = PreparationEventKind::Failed {
            error: "nix build failed".to_owned(),
        };

        writer.record(at(1), found("/nix/store/a"));
        writer.record(at(2), found("/nix/store/a"));
        // A restarted process finds the same artifact.
        let restarted = PreparationLogs::on_system_clock(directory.path())
            .writer(DEPLOYMENT, PreparationStep::EngineBuild);
        restarted.record(at(3), found("/nix/store/a"));
        // A deployment update turns the step to another artifact.
        restarted.record(at(4), found("/nix/store/b"));
        // A cold attempt that finishes is the decision the next pass finds.
        restarted.record(at(5), started("/nix/store/c"));
        restarted.record(at(6), finished);
        restarted.record(at(7), found("/nix/store/c"));
        // A failed attempt leaves no decision, so finding the artifact afterwards is news.
        restarted.record(at(8), started("/nix/store/d"));
        restarted.record(at(9), failed);
        restarted.record(at(10), found("/nix/store/d"));
        restarted.record(at(11), found("/nix/store/d"));

        assert_eq!(
            contents(&read(&logs, PreparationStep::EngineBuild, 20, None).await),
            [
                "Already built: /nix/store/a",
                "Already built: /nix/store/b",
                "Preparing again in a new attempt",
                "Finished in 2.0s",
                "Preparing again in a new attempt",
                "Failed: nix build failed",
                "Already built: /nix/store/d",
            ]
        );
    }

    #[tokio::test]
    async fn each_step_kind_names_the_artifact_it_found_prepared() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let service = PreparationStep::ServiceImage {
            service: "web".to_owned(),
        };
        let found = |step: PreparationStep, artifact: &str| {
            logs.writer(DEPLOYMENT, step).record(
                at(1),
                PreparationEventKind::AlreadyPrepared {
                    artifact: artifact.to_owned(),
                },
            );
        };

        found(
            PreparationStep::ModelDownload,
            "owner/model/weights.gguf@0123",
        );
        found(
            PreparationStep::EngineImage,
            "registry.example/engine@sha256:01",
        );
        found(service.clone(), "registry.example/web@sha256:02");

        assert_eq!(
            contents(&read(&logs, PreparationStep::ModelDownload, 10, None).await),
            ["Already downloaded: owner/model/weights.gguf@0123"]
        );
        assert_eq!(
            contents(&read(&logs, PreparationStep::EngineImage, 10, None).await),
            ["Already present: registry.example/engine@sha256:01"]
        );
        assert_eq!(
            contents(&read(&logs, service, 10, None).await),
            ["Already present: registry.example/web@sha256:02"]
        );
    }

    #[tokio::test]
    async fn a_decision_outlives_the_rotation_that_drops_its_line() {
        let directory = tempfile::tempdir().unwrap();
        // Each record is 29 bytes, so each file holds two of them.
        let logs = PreparationLogs::with_cap(directory.path(), Arc::new(SystemClock), 58);
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineImage);
        let found = PreparationEventKind::AlreadyPrepared {
            artifact: "image".to_owned(),
        };
        writer.record(at(1), found.clone());
        for second in 2..=5 {
            writer.append(
                at(second),
                RuntimeLogStreamKind::Stdout,
                format!("line {second}").as_bytes(),
            );
        }

        writer.record(at(6), found);

        assert_eq!(
            contents(&read(&logs, PreparationStep::EngineImage, 10, None).await),
            [
                "Earlier output of this step was dropped to keep its log within 8 MiB.",
                "line 2",
                "line 3",
                "line 4",
                "line 5",
            ]
        );
    }

    #[tokio::test]
    async fn removal_forgets_the_decisions_with_the_logs() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let found = PreparationEventKind::AlreadyPrepared {
            artifact: STORE_PATH.to_owned(),
        };
        logs.writer(DEPLOYMENT, PreparationStep::EngineBuild)
            .record(at(1), found.clone());
        let decision = directory
            .path()
            .join("preparation-logs/model")
            .join(DEPLOYMENT)
            .join("engine-build.decision");
        assert_eq!(std::fs::read_to_string(&decision).unwrap(), STORE_PATH);

        logs.remove(PreparationOwner::Model, DEPLOYMENT).unwrap();
        assert!(!decision.exists());

        // A reused identity starts over and writes its decision again.
        logs.writer(DEPLOYMENT, PreparationStep::EngineBuild)
            .record(at(2), found);
        assert_eq!(
            contents(&read(&logs, PreparationStep::EngineBuild, 10, None).await),
            [format!("Already built: {STORE_PATH}")]
        );
    }

    #[tokio::test]
    async fn lines_are_split_into_entries_with_strictly_increasing_timestamps() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::ModelDownload);

        writer.append(at(10), RuntimeLogStreamKind::Stdout, b"first\nsecond\n");
        // A backward clock step keeps the order the lines were written in.
        writer.append(at(5), RuntimeLogStreamKind::Stdout, b"after a clock step");
        writer.append(
            at(20),
            RuntimeLogStreamKind::Stdout,
            &vec![b'x'; MAX_LOG_ENTRY_CONTENT_BYTES + 1],
        );

        let history = read(&logs, PreparationStep::ModelDownload, 10, None).await;
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| (entry.timestamp, entry.content.len()))
                .collect::<Vec<_>>(),
            [
                (at(10), 5),
                (at(10) + Duration::from_nanos(1), 6),
                (at(10) + Duration::from_nanos(2), 18),
                (at(20), MAX_LOG_ENTRY_CONTENT_BYTES),
                (at(20) + Duration::from_nanos(1), 1),
            ]
        );
    }

    #[tokio::test]
    async fn a_reopened_store_continues_after_the_retained_entries() {
        let directory = tempfile::tempdir().unwrap();
        PreparationLogs::on_system_clock(directory.path())
            .writer(DEPLOYMENT, PreparationStep::EngineImage)
            .append(at(30), RuntimeLogStreamKind::Stdout, b"before the restart");

        // The restarted process's clock reads earlier than the retained entry.
        let restarted = PreparationLogs::on_system_clock(directory.path());
        restarted
            .writer(DEPLOYMENT, PreparationStep::EngineImage)
            .append(at(3), RuntimeLogStreamKind::Stdout, b"after the restart");

        let history = read(&restarted, PreparationStep::EngineImage, 10, None).await;
        assert_eq!(
            contents(&history),
            ["before the restart", "after the restart"]
        );
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| entry.timestamp)
                .collect::<Vec<_>>(),
            [at(30), at(30) + Duration::from_nanos(1)]
        );
    }

    #[tokio::test]
    async fn rotation_drops_the_oldest_output_and_says_so() {
        let directory = tempfile::tempdir().unwrap();
        // Each record is 29 bytes, so each file holds two of them.
        let logs = PreparationLogs::with_cap(directory.path(), Arc::new(SystemClock), 58);
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        for second in 1..=4 {
            writer.append(
                at(second),
                RuntimeLogStreamKind::Stdout,
                format!("line {second}").as_bytes(),
            );
        }
        let before_drop = read(&logs, PreparationStep::EngineBuild, 10, None).await;

        writer.append(at(5), RuntimeLogStreamKind::Stdout, b"line 5");
        let after_drop = read(&logs, PreparationStep::EngineBuild, 10, None).await;

        assert_eq!(
            contents(&before_drop),
            ["line 1", "line 2", "line 3", "line 4"]
        );
        assert_eq!(
            contents(&after_drop),
            [
                "Earlier output of this step was dropped to keep its log within 8 MiB.",
                "line 3",
                "line 4",
                "line 5"
            ]
        );
        assert_eq!(
            after_drop.entries[0].timestamp,
            at(3) - Duration::from_nanos(1)
        );
        assert!(!after_drop.has_older_output);
    }

    #[tokio::test]
    async fn a_follow_replays_retained_entries_then_receives_new_lines() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"retained");
        let cancellation = CancellationToken::new();
        let mut follow = follow(&logs, PreparationStep::EngineBuild, None, &cancellation);

        assert_eq!(next_content(&mut follow).await, "retained");
        writer.append(at(2), RuntimeLogStreamKind::Stderr, b"appended");
        assert_eq!(next_content(&mut follow).await, "appended");

        cancellation.cancel();
        assert!(follow.next().await.is_none());
    }

    #[tokio::test]
    async fn a_resumed_follow_neither_repeats_nor_skips_entries() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        // Two lines written at the same instant still get distinct timestamps.
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"one");
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"two");
        let cancellation = CancellationToken::new();
        let mut first = follow(&logs, PreparationStep::EngineBuild, None, &cancellation);
        assert_eq!(next_content(&mut first).await, "one");
        let latest = match first.next().await {
            Some(Ok(RuntimeLogEvent::Entry(entry))) => entry.timestamp,
            other => panic!("expected an entry, got {other:?}"),
        };
        drop(first);
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"three");

        let mut resumed = follow(
            &logs,
            PreparationStep::EngineBuild,
            Some(latest),
            &cancellation,
        );
        assert_eq!(next_content(&mut resumed).await, "three");
        writer.append(at(2), RuntimeLogStreamKind::Stdout, b"four");
        assert_eq!(next_content(&mut resumed).await, "four");
    }

    #[tokio::test]
    async fn a_follow_that_falls_behind_reads_the_log_again() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        let cancellation = CancellationToken::new();
        let mut follow = follow(&logs, PreparationStep::EngineBuild, None, &cancellation);
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"line 0");
        assert_eq!(next_content(&mut follow).await, "line 0");

        let total = FOLLOW_BUFFER + 10;
        for line in 1..=total {
            writer.append(
                at(1),
                RuntimeLogStreamKind::Stdout,
                format!("line {line}").as_bytes(),
            );
        }

        let mut received = Vec::new();
        while received.len() < total {
            received.push(next_content(&mut follow).await);
        }
        assert_eq!(
            received,
            (1..=total)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn removal_ends_follows_drops_late_writes_and_leaves_a_reused_identity_empty() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        writer.append(at(1), RuntimeLogStreamKind::Stdout, b"first attempt");
        let other = logs.writer("model-two", PreparationStep::EngineBuild);
        other.append(at(1), RuntimeLogStreamKind::Stdout, b"kept");
        let cancellation = CancellationToken::new();
        let mut follow = follow(&logs, PreparationStep::EngineBuild, None, &cancellation);
        assert_eq!(next_content(&mut follow).await, "first attempt");

        logs.remove(PreparationOwner::Model, DEPLOYMENT).unwrap();
        writer.append(at(2), RuntimeLogStreamKind::Stdout, b"late line");

        assert!(matches!(
            follow.next().await,
            Some(Ok(RuntimeLogEvent::SourceUnavailable(source))) if source.name == "prepare:engine-build"
        ));
        assert!(follow.next().await.is_none());
        assert!(
            !directory
                .path()
                .join("preparation-logs/model")
                .join(DEPLOYMENT)
                .exists()
        );
        assert!(contents(&read(&logs, PreparationStep::EngineBuild, 10, None).await).is_empty());
        assert_eq!(
            logs.select(PreparationOwner::Model, DEPLOYMENT, &[], &model_steps(), 1),
            SourceSelection {
                steps: Vec::new(),
                workload: Some(Vec::new())
            }
        );
        assert_eq!(
            contents(
                &PreparationLogs::on_system_clock(directory.path())
                    .read(
                        "model-two",
                        &[PreparationStep::EngineBuild],
                        &query(10, None)
                    )
                    .await
                    .unwrap()[0]
                    .result
                    .as_ref()
                    .unwrap()
                    .clone()
            ),
            ["kept"]
        );
    }

    #[test]
    fn an_attempt_begun_before_a_removal_writes_nothing_after_it() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let removals = logs.store.removals(PreparationOwner::Model, DEPLOYMENT);
        let started = logs
            .store
            .writer(DEPLOYMENT, &PreparationStep::ModelDownload, removals)
            .expect("a current attempt writes");
        PreparationStepWriter { log: started }.append(
            at(1),
            RuntimeLogStreamKind::Stdout,
            b"downloading",
        );

        logs.remove(PreparationOwner::Model, DEPLOYMENT).unwrap();

        // The cancelled attempt reaches its next step only after the removal.
        assert!(
            logs.store
                .writer(DEPLOYMENT, &PreparationStep::EngineBuild, removals)
                .is_none()
        );
        assert!(
            !directory
                .path()
                .join("preparation-logs/model")
                .join(DEPLOYMENT)
                .exists()
        );
        let next_attempt = logs.store.removals(PreparationOwner::Model, DEPLOYMENT);
        assert!(
            logs.store
                .writer(DEPLOYMENT, &PreparationStep::EngineBuild, next_attempt)
                .is_some()
        );
        // Another owner's deployment with the same identity is not affected.
        assert_eq!(
            logs.store
                .removals(PreparationOwner::Application, DEPLOYMENT),
            0
        );
    }

    #[tokio::test]
    async fn a_follow_of_every_source_lists_each_step_once_it_starts() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        // The download keeps writing in one activity, which a step already followed is.
        let download = logs.writer(DEPLOYMENT, PreparationStep::ModelDownload);
        download.append(at(1), RuntimeLogStreamKind::Stdout, b"downloading");
        let cancellation = CancellationToken::new();
        let mut late = logs.follow_started(
            PreparationOwner::Model,
            DEPLOYMENT,
            model_steps().to_vec(),
            &[PreparationStep::ModelDownload],
            1,
            &DeploymentLogFollowQuery::new(Vec::new(), BTreeMap::new(), cancellation.clone()),
        );
        // Another deployment's steps and a step already followed are never listed.
        logs.writer("model-two", PreparationStep::EngineBuild)
            .append(at(2), RuntimeLogStreamKind::Stdout, b"elsewhere");
        download.append(at(3), RuntimeLogStreamKind::Stdout, b"still downloading");
        tokio::task::yield_now().await;
        assert!(late.try_recv().is_err(), "no step started yet");

        logs.writer(DEPLOYMENT, PreparationStep::EngineBuild)
            .append(at(4), RuntimeLogStreamKind::Stderr, b"building");
        let listed = tokio::time::timeout(Duration::from_secs(10), late.recv())
            .await
            .expect("late step deadline")
            .expect("a late step");
        assert_eq!(listed.source, step_source(&PreparationStep::EngineBuild));
        let mut stream = listed.result.expect("an open step follow");
        assert_eq!(next_content(&mut stream).await, "building");

        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), late.recv())
                .await
                .expect("closing deadline")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_step_follow_ends_once_its_attempt_ends_and_at_once_for_a_finished_step() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        writer.record(
            at(1),
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
        );
        let cancellation = CancellationToken::new();
        let mut running = follow(&logs, PreparationStep::EngineBuild, None, &cancellation);
        assert_eq!(next_content(&mut running).await, "Preparing");

        writer.record(at(2), line("building"));
        writer.record(
            at(3),
            PreparationEventKind::Finished {
                duration: Duration::from_secs(2),
            },
        );
        let running_rest = remaining_contents(running).await;
        let finished = remaining_contents(follow(
            &logs,
            PreparationStep::EngineBuild,
            None,
            &cancellation,
        ))
        .await;
        let failing = logs.writer(DEPLOYMENT, PreparationStep::ModelDownload);
        failing.record(
            at(4),
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
        );
        let mut failed = follow(&logs, PreparationStep::ModelDownload, None, &cancellation);
        assert_eq!(next_content(&mut failed).await, "Preparing");
        failing.record(
            at(5),
            PreparationEventKind::Failed {
                error: "unreachable".to_owned(),
            },
        );

        assert_eq!(running_rest, ["building", "Finished in 2.0s"]);
        assert_eq!(
            finished,
            ["Preparing", "building", "Finished in 2.0s"],
            "a finished step replays its output, then ends"
        );
        assert_eq!(remaining_contents(failed).await, ["Failed: unreachable"]);
    }

    #[tokio::test]
    async fn a_follow_of_every_source_lists_a_step_again_when_it_starts_again() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let writer = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        writer.record(
            at(1),
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
        );
        writer.record(
            at(2),
            PreparationEventKind::Failed {
                error: "nix build failed".to_owned(),
            },
        );
        // The opened step and the workload sources fill the source limit.
        let mut late = logs.follow_started(
            PreparationOwner::Model,
            DEPLOYMENT,
            model_steps().to_vec(),
            &[PreparationStep::EngineBuild],
            MAX_RUNTIME_LOG_SOURCES - 1,
            &DeploymentLogFollowQuery::new(
                Vec::new(),
                BTreeMap::from([(
                    step_source(&PreparationStep::EngineBuild).name,
                    at(2) - Duration::from_nanos(1),
                )]),
                CancellationToken::new(),
            ),
        );

        writer.record(
            at(3),
            PreparationEventKind::Started {
                artifact: STORE_PATH.to_owned(),
            },
        );
        let listed = tokio::time::timeout(Duration::from_secs(10), late.recv())
            .await
            .expect("late step deadline")
            .expect("the step listed again");
        writer.record(
            at(4),
            PreparationEventKind::Finished {
                duration: Duration::from_secs(1),
            },
        );

        assert_eq!(listed.source, step_source(&PreparationStep::EngineBuild));
        assert_eq!(
            remaining_contents(listed.result.expect("listed again within the source limit")).await,
            [
                "Failed: nix build failed",
                "Preparing again in a new attempt",
                "Finished in 1.0s"
            ],
            "the step resumes after its resume position and ends with its attempt"
        );
    }

    #[tokio::test]
    async fn a_step_starting_past_the_source_limit_is_listed_as_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        // One opened step and the workload sources leave room for one more source.
        let mut late = logs.follow_started(
            PreparationOwner::Model,
            DEPLOYMENT,
            model_steps().to_vec(),
            &[PreparationStep::ModelDownload],
            MAX_RUNTIME_LOG_SOURCES - 2,
            &DeploymentLogFollowQuery::new(Vec::new(), BTreeMap::new(), CancellationToken::new()),
        );
        let mut next_listed = async || {
            tokio::time::timeout(Duration::from_secs(10), late.recv())
                .await
                .expect("late step deadline")
                .expect("a late step")
        };

        logs.writer(DEPLOYMENT, PreparationStep::EngineBuild)
            .append(at(1), RuntimeLogStreamKind::Stdout, b"building");
        let followed = next_listed().await;
        logs.writer(DEPLOYMENT, PreparationStep::EngineImage)
            .append(at(2), RuntimeLogStreamKind::Stdout, b"loading");
        let over_limit = next_listed().await;

        assert_eq!(followed.source, step_source(&PreparationStep::EngineBuild));
        assert!(followed.result.is_ok());
        assert_eq!(
            over_limit.source,
            step_source(&PreparationStep::EngineImage)
        );
        assert!(matches!(
            over_limit.result,
            Err(RuntimeBackendError::LogSourceLimitReached)
        ));
    }

    #[tokio::test]
    async fn a_follow_opened_on_a_removed_step_ends_at_once() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        // A follow that took the step just before the removal subscribes only after it.
        let log = logs.store.step(DEPLOYMENT, &PreparationStep::EngineBuild);
        logs.remove(PreparationOwner::Model, DEPLOYMENT).unwrap();

        let mut stream = log.follow(
            step_source(&PreparationStep::EngineBuild),
            RuntimeLogFollowQuery::new(None, CancellationToken::new()),
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(10), stream.next())
                .await
                .expect("follow event deadline"),
            Some(Ok(RuntimeLogEvent::SourceUnavailable(_)))
        ));
    }

    #[test]
    fn steps_no_one_holds_leave_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        let held = logs.writer(DEPLOYMENT, PreparationStep::EngineBuild);
        drop(
            logs.store
                .step("model-read-once", &PreparationStep::EngineBuild),
        );

        let _next = logs.store.step("model-two", &PreparationStep::EngineBuild);

        let keys = lock(&logs.store.state)
            .steps
            .keys()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        assert_eq!(keys, [DEPLOYMENT, "model-two"]);
        drop(held);
    }

    #[test]
    fn a_service_name_that_is_not_an_identifier_stays_inside_its_directory() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());

        logs.writer(
            "app-one",
            PreparationStep::ServiceBuild {
                service: "../web".to_owned(),
            },
        )
        .append(at(1), RuntimeLogStreamKind::Stdout, b"contained");

        assert!(
            directory
                .path()
                .join("preparation-logs/application/app-one/_2e2e2f776562.build.log")
                .exists()
        );
    }

    #[test]
    fn a_selection_without_names_lists_started_known_steps_in_start_order() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());
        logs.writer(
            "app-one",
            PreparationStep::ServiceImage {
                service: "web".to_owned(),
            },
        )
        .append(at(2), RuntimeLogStreamKind::Stdout, b"image");
        logs.writer(
            "app-one",
            PreparationStep::ServiceBuild {
                service: "web".to_owned(),
            },
        )
        .append(at(1), RuntimeLogStreamKind::Stdout, b"build");
        logs.writer(
            "app-one",
            PreparationStep::ServiceBuild {
                service: "removed".to_owned(),
            },
        )
        .append(at(0), RuntimeLogStreamKind::Stdout, b"no longer declared");
        let known = [service_steps("db").to_vec(), service_steps("web").to_vec()].concat();

        let all = logs.select(PreparationOwner::Application, "app-one", &[], &known, 2);
        let capped = logs.select(
            PreparationOwner::Application,
            "app-one",
            &[],
            &known,
            MAX_RUNTIME_LOG_SOURCES - 1,
        );
        let named = logs.select(
            PreparationOwner::Application,
            "app-one",
            &[
                "web".to_owned(),
                "prepare:db:build".to_owned(),
                "prepare:download".to_owned(),
            ],
            &known,
            2,
        );
        let steps_only = logs.select(
            PreparationOwner::Application,
            "app-one",
            &["prepare:web:image".to_owned()],
            &known,
            2,
        );

        assert_eq!(
            all,
            SourceSelection {
                steps: vec![
                    PreparationStep::ServiceBuild {
                        service: "web".to_owned()
                    },
                    PreparationStep::ServiceImage {
                        service: "web".to_owned()
                    },
                ],
                workload: Some(Vec::new()),
            }
        );
        assert_eq!(
            capped.steps,
            [PreparationStep::ServiceBuild {
                service: "web".to_owned()
            }]
        );
        assert_eq!(
            named,
            SourceSelection {
                steps: vec![PreparationStep::ServiceBuild {
                    service: "db".to_owned()
                }],
                workload: Some(vec!["web".to_owned(), "prepare:download".to_owned()]),
            }
        );
        assert_eq!(
            steps_only,
            SourceSelection {
                steps: vec![PreparationStep::ServiceImage {
                    service: "web".to_owned()
                }],
                workload: None,
            }
        );
    }

    #[test]
    fn step_sources_round_trip_through_their_names() {
        let steps = [model_steps().to_vec(), service_steps("open-webui").to_vec()].concat();

        let sources = steps.iter().map(step_source).collect::<Vec<_>>();

        assert_eq!(
            sources
                .iter()
                .map(|source| (source.name.as_str(), source.label.as_str()))
                .collect::<Vec<_>>(),
            [
                ("prepare:download", "Model download"),
                ("prepare:engine-build", "Engine build"),
                ("prepare:engine-image", "Engine image"),
                ("prepare:open-webui:build", "open-webui build"),
                ("prepare:open-webui:image", "open-webui image"),
            ]
        );
        assert_eq!(
            sources
                .iter()
                .map(|source| step_from_source_name(&source.name))
                .collect::<Vec<_>>(),
            steps.into_iter().map(Some).collect::<Vec<_>>()
        );
        assert_eq!(step_from_source_name("model"), None);
        assert_eq!(step_from_source_name("prepare:Web:build"), None);
        assert_eq!(step_from_source_name("prepare:web:run"), None);
    }

    #[test]
    fn an_identity_that_is_not_an_identifier_stays_inside_the_store() {
        let directory = tempfile::tempdir().unwrap();
        let logs = PreparationLogs::on_system_clock(directory.path());

        logs.writer("../escape", PreparationStep::EngineBuild)
            .append(at(1), RuntimeLogStreamKind::Stdout, b"contained");

        assert!(
            directory
                .path()
                .join("preparation-logs/model/_2e2e2f657363617065/engine-build.log")
                .exists()
        );
    }

    fn at(second: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000 + second)
    }

    fn query(maximum: u16, before: Option<SystemTime>) -> RuntimeLogQuery {
        RuntimeLogQuery::new(NonZeroU16::new(maximum).unwrap(), before).unwrap()
    }

    async fn read(
        logs: &PreparationLogs,
        step: PreparationStep,
        maximum: u16,
        before: Option<SystemTime>,
    ) -> RuntimeLogHistory {
        let mut histories = logs
            .read(DEPLOYMENT, &[step], &query(maximum, before))
            .await
            .unwrap();
        histories.remove(0).result.unwrap()
    }

    fn follow(
        logs: &PreparationLogs,
        step: PreparationStep,
        since: Option<SystemTime>,
        cancellation: &CancellationToken,
    ) -> RuntimeLogStream {
        let positions = since
            .map(|since| BTreeMap::from([(step_source(&step).name, since)]))
            .unwrap_or_default();
        logs.follow(DEPLOYMENT, &[step], &positions, cancellation)
            .remove(0)
            .result
            .unwrap()
    }

    async fn next_content(follow: &mut RuntimeLogStream) -> String {
        let event = tokio::time::timeout(Duration::from_secs(10), follow.next())
            .await
            .expect("follow event deadline");
        match event {
            Some(Ok(RuntimeLogEvent::Entry(entry))) => String::from_utf8(entry.content).unwrap(),
            other => panic!("expected an entry, got {other:?}"),
        }
    }

    /// The contents of every entry a follow still delivers, until it ends.
    async fn remaining_contents(follow: RuntimeLogStream) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(10), follow.collect::<Vec<_>>())
            .await
            .expect("the follow ends")
            .into_iter()
            .map(|event| match event {
                Ok(RuntimeLogEvent::Entry(entry)) => String::from_utf8(entry.content).unwrap(),
                other => panic!("expected an entry, got {other:?}"),
            })
            .collect()
    }

    fn line(text: &str) -> PreparationEventKind {
        PreparationEventKind::Line {
            stream: OutputStream::Stderr,
            text: text.to_owned(),
        }
    }

    fn contents(history: &RuntimeLogHistory) -> Vec<String> {
        history
            .entries
            .iter()
            .map(|entry| String::from_utf8(entry.content.clone()).unwrap())
            .collect()
    }
}
