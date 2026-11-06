//! Native runtime backend for one execution target: creates, starts, inspects, stops, removes,
//! and rediscovers workloads that run as launcher-held host processes, for example after the
//! server restarts while its workloads keep running.
//!
//! Every mutation first re-reads the workload's launch record and re-checks its ownership
//! evidence, so the backend never acts on a workload of another state root or on records it
//! cannot trust; discovery fails as a whole on untrustworthy records instead of skipping them.
//! An owned workload whose identity no longer recomputes the same way is reported under a
//! reconcile generation no launch derives, so its owner replaces it; the backend never removes
//! it on its own.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime},
};

use dotcore_domain::{ExecutionPlatform, host_process::HostProcessIdentity, timing::Timing};
use dotcore_launcher::{
    supervisor::{ScannedWorkload, Supervisor, SupervisorError, WorkloadState},
    workload::{ExitStatus, LaunchSpec, Lifetime, PROCESS_FILE, ProcessRecord, StoredRecord},
};
use dotcore_runtime::{
    LoopbackEndpoint, ObservedWorkload, ProcessMemory, ProcessMemoryObservation,
    ProcessMemoryUnavailable, RuntimeBackend, RuntimeBackendError, RuntimeBackendId, RuntimeFuture,
    RuntimeLogFuture, RuntimeLogHistory, RuntimeLogStream, RuntimeWorkloadLogFollowQuery,
    RuntimeWorkloadLogQuery, WorkloadCorrelation, WorkloadId, WorkloadLaunch, WorkloadObservation,
    WorkloadServiceBinding,
};
use tracing::{info, warn};

use super::{
    NativeRuntimeConfig, NativeWorkloadLifetime,
    launch::{PreparedLaunch, prepare_launch, resolve_command, workload_id},
    memory::{HostProcessMemoryFacts, process_tree_memory},
    ownership::{NativeOwnershipError, NativeWorkloadClass, OwnershipEvidence},
    ports::assign_ports,
    record::NativeOwnerRecord,
    state_root::OpenedStateRoot,
};

/// Canonical runtime backend identifier for native workloads.
pub const BACKEND_ID: &str = "runtime/native";

/// Interval between the checks of a new workload while its ports settle.
const PORT_SETTLE_POLL: Duration = Duration::from_millis(50);

/// Longest wait for a connection that tells whether a loopback port is in use.
const PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);

/// Runtime backend that runs workloads of one execution target as launcher-held host processes.
pub struct NativeRuntimeBackend {
    config: NativeRuntimeConfig,
    state_root: OpenedStateRoot,
    pub(super) supervisor: Supervisor,
    /// Owned workloads found by discovery or created here, by workload id.
    known: Mutex<BTreeMap<String, KnownWorkload>>,
    /// How long a create watches a new workload for an early exit caused by a taken port;
    /// engines bind their service ports early in startup, and a later exit is reported as an
    /// ordinary stop.
    port_settle_window: Duration,
}

impl NativeRuntimeBackend {
    /// Opens the execution target's state directory and binds workloads to the launcher binary
    /// at `launcher`, watching new workloads' ports for the production settle window.
    pub fn new(
        config: NativeRuntimeConfig,
        launcher: PathBuf,
    ) -> Result<Self, RuntimeBackendError> {
        let state_root = config
            .state_root
            .open(&config.execution_target_id)
            .map_err(|error| {
                RuntimeBackendError::native_with_source("open native workload state", error)
            })?;
        let supervisor =
            Supervisor::new(&state_root.target_directory, launcher).map_err(|error| {
                RuntimeBackendError::native_with_source("open native workload supervisor", error)
            })?;
        Ok(Self {
            config,
            state_root,
            supervisor,
            known: Mutex::new(BTreeMap::new()),
            port_settle_window: Timing::production().native_port_settle_window,
        })
    }

    /// Watches each new workload's ports for `window` instead, as the backend's timing sets it.
    pub fn with_port_settle_window(mut self, window: Duration) -> Self {
        self.port_settle_window = window;
        self
    }

    /// Counts every workload directory of this execution target, including creates in flight and
    /// workloads whose evidence names another state root, which discovery does not report.
    pub async fn workload_directory_count(&self) -> Result<usize, RuntimeBackendError> {
        let scanned = self
            .supervisor
            .scan()
            .await
            .map_err(|error| supervisor_error("scan native workloads", error))?;
        Ok(scanned.len())
    }

    /// Returns the process facts of one known workload after re-reading its ownership evidence:
    /// the recorded launch intent, the command it runs, and its host process while it runs.
    pub async fn workload_process(
        &self,
        workload_id: &WorkloadId,
    ) -> Result<NativeWorkloadProcess, RuntimeBackendError> {
        let id = &workload_id.0;
        let known = self
            .live_known_workload(id)
            .await?
            .ok_or_else(|| unknown_workload(id))?;
        let running = matches!(
            self.workload_state(id).await?,
            Some(WorkloadState::Running { .. })
        );
        let process_id = if running {
            self.host_process(id)
                .await
                .map(|process| process.process_id)
        } else {
            None
        };
        let command = resolve_command(&known.record.launch.spec, &self.config, &known.record.ports)
            .map_err(|error| {
                RuntimeBackendError::native_with_source("resolve native workload command", error)
            })?;
        Ok(NativeWorkloadProcess {
            launch_fingerprint: known.record.launch.exact_intent_fingerprint(),
            process_id,
            command: std::iter::once(command.executable)
                .chain(command.arguments)
                .collect(),
            loopback_endpoints: known.serving_identity().loopback_endpoints,
        })
    }

    async fn discover(&self) -> Result<Vec<WorkloadObservation>, RuntimeBackendError> {
        let scanned = self
            .supervisor
            .scan()
            .await
            .map_err(|error| supervisor_error("scan native workloads", error))?;
        let mut states = BTreeMap::new();
        let mut payloads = Vec::new();
        // A create still in flight, here or before a restart, is not reported: its id stays
        // blocked by its directory until it finishes or becomes an interrupted create.
        let mut creating = BTreeSet::new();
        let mut scanned_ids = BTreeSet::new();
        for ScannedWorkload { id, state } in scanned {
            if matches!(state, WorkloadState::Creating) {
                creating.insert(id);
                continue;
            }
            scanned_ids.insert(id.clone());
            match state {
                WorkloadState::Creating => {}
                WorkloadState::Interrupted => {
                    tolerate_gone(self.supervisor.remove_interrupted(&id).await).map_err(
                        |error| {
                            supervisor_error(
                                format!("remove interrupted native workload create {id}"),
                                error,
                            )
                        },
                    )?;
                    info!(event_kind = "native_workload_interrupted_create_removed", workload_id = %id, "removed an interrupted native workload create");
                }
                WorkloadState::Inconsistent { problem } => {
                    return Err(self.untrusted(&id, NativeOwnershipError::Untrusted(problem)));
                }
                WorkloadState::Running { ref payload } => {
                    payloads.push((id.clone(), Ok(payload.spec.clone())));
                    states.insert(id, state);
                }
                state => {
                    payloads.push((id.clone(), self.read_payload(&id).await?));
                    states.insert(id, state);
                }
            }
        }
        let classified = OwnershipEvidence::new(&self.config, &self.state_root)
            .classify_all(payloads)
            .map_err(|error| {
                RuntimeBackendError::Ownership(anyhow::Error::new(error).context(format!(
                    "native workloads under {} carry conflicting ownership evidence",
                    self.state_root.target_directory.display()
                )))
            })?;

        let mut known = BTreeMap::new();
        let mut discovered = Vec::new();
        for workload in classified {
            let Some(state) = states.remove(&workload.id) else {
                continue;
            };
            let known_workload = match workload.class {
                NativeWorkloadClass::Owned(record) => {
                    KnownWorkload::new(&workload.id, record, false)
                }
                NativeWorkloadClass::OwnedStale { record, reason } => {
                    warn!(event_kind = "native_workload_stale", workload_id = %workload.id, reason = %reason, "owned native workload is stale and reported for replacement");
                    KnownWorkload::new(&workload.id, record, true)
                }
                NativeWorkloadClass::Foreign { state_root } => {
                    warn!(event_kind = "native_workload_foreign", workload_id = %workload.id, recorded_state_root = %state_root, "native workload belongs to another state root and is left untouched");
                    continue;
                }
                NativeWorkloadClass::Malformed(error) => {
                    return Err(self.untrusted(&workload.id, error));
                }
            };
            known.insert(workload.id.clone(), known_workload.clone());
            discovered.push((known_workload, state));
        }
        {
            // Merged rather than replaced: a create or relaunch that finished after the scan
            // keeps its entry, while workloads the scan saw are known exactly as classified.
            let mut registry = self.known.lock().expect("native workload registry");
            registry.retain(|id, _| {
                creating.contains(id)
                    || (!scanned_ids.contains(id) && self.workload_directory(id).exists())
            });
            registry.extend(known);
        }

        let mut observations = Vec::with_capacity(discovered.len());
        for (known, state) in discovered {
            let observation = match state {
                // A launcher still watching an earlier server process is drained, never adopted.
                WorkloadState::Terminating => {
                    self.stop_and_remove(&known.identity.workload_id.0).await?;
                    self.forget(&known.identity.workload_id.0);
                    WorkloadObservation::Absent(known.identity)
                }
                state => self.observe(&known, state).await?,
            };
            observations.push(observation);
        }
        Ok(observations)
    }

    async fn create(
        &self,
        launch: &WorkloadLaunch,
    ) -> Result<WorkloadObservation, RuntimeBackendError> {
        launch.spec.validate_runtime_platform(&host_platform()?)?;
        let id = workload_id(&launch.correlation);
        if self.known_workload(&id).is_some() {
            return self.inspect(&id).await;
        }
        let ports = assign_ports(&launch.spec.ports, &BTreeMap::new()).map_err(|error| {
            RuntimeBackendError::native_with_source("assign native workload loopback ports", error)
        })?;
        self.launch(&id, launch, ports).await?;
        info!(event_kind = "native_workload_created", workload_id = %id, "created native workload");
        self.inspect(&id).await
    }

    /// Relaunches a finished workload in place from its persisted owner record, keeping its
    /// assigned ports while they are free.
    async fn start(&self, id: &str) -> Result<WorkloadObservation, RuntimeBackendError> {
        let Some(known) = self.live_known_workload(id).await? else {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "native workload {id} no longer exists"
            )));
        };
        if known.stale {
            return Err(RuntimeBackendError::invalid_launch(format!(
                "native workload {id} is stale and must be replaced instead of restarted"
            )));
        }
        match self.workload_state(id).await? {
            Some(WorkloadState::Running { .. }) => return self.inspect(id).await,
            Some(
                WorkloadState::Exited { .. } | WorkloadState::Lost | WorkloadState::Absent { .. },
            ) => {}
            _ => {
                return Err(RuntimeBackendError::invalid_launch(format!(
                    "native workload {id} cannot be started in its current state"
                )));
            }
        }
        let ports = assign_ports(&known.record.launch.spec.ports, &known.record.ports).map_err(
            |error| {
                RuntimeBackendError::native_with_source(
                    "assign native workload loopback ports",
                    error,
                )
            },
        )?;
        // Validated before the finished workload is removed, so a relaunch that cannot start
        // keeps the exit status and logs.
        let prepared = self.prepare(&known.record.launch, ports)?;
        tolerate_gone(self.supervisor.remove(id).await).map_err(|error| {
            supervisor_error(format!("remove finished native workload {id}"), error)
        })?;
        self.launch_prepared(id, prepared).await?;
        info!(event_kind = "native_workload_restarted", workload_id = %id, "relaunched native workload from its owner record");
        self.inspect(id).await
    }

    async fn inspect(&self, id: &str) -> Result<WorkloadObservation, RuntimeBackendError> {
        let known = self
            .known_workload(id)
            .ok_or_else(|| unknown_workload(id))?;
        let Some(live) = self.live_known_workload(id).await? else {
            return Ok(WorkloadObservation::Absent(known.identity));
        };
        let Some(state) = self.workload_state(id).await? else {
            self.forget(id);
            return Ok(WorkloadObservation::Absent(live.identity));
        };
        self.observe(&live, state).await
    }

    async fn stop(&self, id: &str) -> Result<(), RuntimeBackendError> {
        if self.live_known_workload(id).await?.is_none() {
            return Ok(());
        }
        self.supervisor
            .stop(id)
            .await
            .map_err(|error| supervisor_error(format!("stop native workload {id}"), error))?;
        info!(event_kind = "native_workload_stopped", workload_id = %id, "stopped native workload");
        Ok(())
    }

    async fn remove(&self, id: &str) -> Result<(), RuntimeBackendError> {
        if self.live_known_workload(id).await?.is_some() {
            self.stop_and_remove(id).await?;
            info!(event_kind = "native_workload_removed", workload_id = %id, "removed native workload");
        }
        self.forget(id);
        Ok(())
    }

    /// Prepares and creates one workload directory and its launcher, then records the workload
    /// as known.
    async fn launch(
        &self,
        id: &str,
        launch: &WorkloadLaunch,
        ports: BTreeMap<String, u16>,
    ) -> Result<(), RuntimeBackendError> {
        let prepared = self.prepare(launch, ports)?;
        self.launch_prepared(id, prepared).await
    }

    /// Validates a launch against the host and builds its launcher request.
    fn prepare(
        &self,
        launch: &WorkloadLaunch,
        ports: BTreeMap<String, u16>,
    ) -> Result<PreparedLaunch, RuntimeBackendError> {
        Ok(prepare_launch(
            &self.config,
            &self.state_root,
            launch,
            ports,
            self.lifetime(),
        )?)
    }

    /// Creates the workload directory and launcher of a prepared launch, records the workload
    /// as known, and watches its ports settle.
    async fn launch_prepared(
        &self,
        id: &str,
        prepared: PreparedLaunch,
    ) -> Result<(), RuntimeBackendError> {
        self.supervisor
            .create(&prepared.workload_id, &prepared.spec)
            .await
            .map_err(|error| supervisor_error(format!("create native workload {id}"), error))?;
        let ports = prepared.record.ports.clone();
        self.known.lock().expect("native workload registry").insert(
            id.to_owned(),
            KnownWorkload::new(id, prepared.record, false),
        );
        self.settle_ports(id, &ports).await
    }

    /// Watches a just-launched workload for a short while. When it exits while another process
    /// accepts connections on one of its assigned ports, the port was taken between assignment
    /// and the engine's bind: the workload is removed and the create fails, so a retried create
    /// assigns new ports.
    async fn settle_ports(
        &self,
        id: &str,
        ports: &BTreeMap<String, u16>,
    ) -> Result<(), RuntimeBackendError> {
        if ports.is_empty() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.port_settle_window;
        loop {
            if matches!(
                self.workload_state(id).await?,
                Some(WorkloadState::Exited { .. } | WorkloadState::Lost)
            ) {
                for port in ports.values() {
                    if accepts_connections(*port).await {
                        self.stop_and_remove(id).await?;
                        self.forget(id);
                        warn!(event_kind = "native_workload_port_taken", workload_id = %id, port, "native workload exited because another process took its assigned loopback port");
                        return Err(RuntimeBackendError::Native(anyhow::anyhow!(
                            "native workload {id} exited because loopback port {port} is in use by \
                             another process; a retried create assigns new ports"
                        )));
                    }
                }
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(());
            }
            tokio::time::sleep(PORT_SETTLE_POLL).await;
        }
    }

    /// Stops a workload when it still runs, then removes its directory, whatever its state.
    async fn stop_and_remove(&self, id: &str) -> Result<(), RuntimeBackendError> {
        stop_and_remove(&self.supervisor, &self.state_root.target_directory, id).await
    }

    /// Maps a launcher state of a known workload to an observation. A conclusively absent
    /// workload's directory is removed.
    async fn observe(
        &self,
        known: &KnownWorkload,
        state: WorkloadState,
    ) -> Result<WorkloadObservation, RuntimeBackendError> {
        let id = &known.identity.workload_id.0;
        let identity = known.identity.clone();
        Ok(match state {
            WorkloadState::Running { .. } => {
                let mut serving = known.serving_identity();
                serving.host_process = self.host_process(id).await;
                WorkloadObservation::Running(serving)
            }
            WorkloadState::Terminating => WorkloadObservation::Stopping(identity),
            WorkloadState::Exited { exit } => WorkloadObservation::Stopped {
                workload: identity,
                exit_status: exit.status.map(|status| match status {
                    ExitStatus::Code(code) => i64::from(code),
                    ExitStatus::Signal(signal) => 128 + i64::from(signal),
                }),
            },
            WorkloadState::Lost => WorkloadObservation::stopped(identity),
            WorkloadState::Absent { .. } => {
                tolerate_gone(self.supervisor.remove(id).await).map_err(|error| {
                    supervisor_error(format!("remove absent native workload {id}"), error)
                })?;
                self.forget(id);
                WorkloadObservation::Absent(identity)
            }
            // A known workload is only creating while this backend relaunches it.
            WorkloadState::Creating => WorkloadObservation::Starting(identity),
            // A create that never recorded its launcher started nothing.
            WorkloadState::Interrupted => {
                tolerate_gone(self.supervisor.remove_interrupted(id).await).map_err(|error| {
                    supervisor_error(format!("remove interrupted native workload {id}"), error)
                })?;
                self.forget(id);
                WorkloadObservation::Absent(identity)
            }
            WorkloadState::Inconsistent { problem } => {
                return Err(self.untrusted(id, NativeOwnershipError::Untrusted(problem)));
            }
        })
    }

    /// Re-reads a known workload's launch record and returns the workload while its ownership
    /// evidence still matches, or `None` once its directory is gone.
    async fn live_known_workload(
        &self,
        id: &str,
    ) -> Result<Option<KnownWorkload>, RuntimeBackendError> {
        let known = self
            .known_workload(id)
            .ok_or_else(|| unknown_workload(id))?;
        match self.workload_state(id).await? {
            None => {
                self.forget(id);
                return Ok(None);
            }
            // The launch record of a create in flight is not published yet; the known entry
            // came from this backend's own create or relaunch.
            Some(WorkloadState::Creating | WorkloadState::Interrupted) => return Ok(Some(known)),
            Some(_) => {}
        }
        let payload = self.read_payload(id).await?;
        let evidence = OwnershipEvidence::new(&self.config, &self.state_root);
        let class = match payload {
            Ok(spec) => evidence.classify(id, Ok(&spec)),
            Err(problem) => evidence.classify(id, Err(problem)),
        };
        match class {
            NativeWorkloadClass::Malformed(error) => Err(self.untrusted(id, error)),
            class => match class.owned_record() {
                Some(record) if record.launch.correlation == known.record.launch.correlation => {
                    Ok(Some(known))
                }
                _ => Err(RuntimeBackendError::Ownership(anyhow::anyhow!(
                    "native workload {id} ownership changed after discovery"
                ))),
            },
        }
    }

    /// Classifies one workload directory, or `None` when it does not exist.
    async fn workload_state(&self, id: &str) -> Result<Option<WorkloadState>, RuntimeBackendError> {
        match self.supervisor.inspect(id).await {
            Ok(state) => Ok(Some(state)),
            Err(SupervisorError::NotFound { .. }) => Ok(None),
            Err(error) => Err(supervisor_error(
                format!("inspect native workload {id}"),
                error,
            )),
        }
    }

    /// Reads a workload's verified launch specification, or the reason it cannot be trusted.
    async fn read_payload(
        &self,
        id: &str,
    ) -> Result<Result<LaunchSpec, dotcore_launcher::supervisor::Inconsistency>, RuntimeBackendError>
    {
        match self.supervisor.payload(id).await {
            Ok(payload) => Ok(Ok(payload.spec)),
            Err(SupervisorError::Inconsistent(problem)) => Ok(Err(problem)),
            Err(error) => Err(supervisor_error(
                format!("read native workload {id} launch record"),
                error,
            )),
        }
    }

    /// Fails with not created unless the workload still exists with its ownership evidence.
    pub(super) async fn require_live_workload(&self, id: &str) -> Result<(), RuntimeBackendError> {
        self.live_known_workload(id)
            .await?
            .map(|_| ())
            .ok_or_else(RuntimeBackendError::workload_not_created)
    }

    /// Sums the memory of the running engine process and its descendants, read off the async
    /// executor, naming the engine process measured; a workload that is not running has no
    /// figure.
    async fn process_memory(
        &self,
        id: &str,
    ) -> Result<ProcessMemoryObservation, RuntimeBackendError> {
        let running = self.known_workload(id).is_some()
            && self.live_known_workload(id).await?.is_some()
            && matches!(
                self.workload_state(id).await?,
                Some(WorkloadState::Running { .. })
            );
        let engine = if running {
            self.host_process(id).await
        } else {
            None
        };
        let memory = match engine.clone() {
            None => ProcessMemory::Unavailable(ProcessMemoryUnavailable::NotRunning),
            Some(engine) => tokio::task::spawn_blocking(move || {
                process_tree_memory(
                    &HostProcessMemoryFacts,
                    engine.process_id,
                    engine.process_start_token,
                )
            })
            .await
            .map_err(|error| {
                RuntimeBackendError::native_with_source("measure native workload memory", error)
            })?
            .map_or_else(ProcessMemory::Unavailable, |bytes| {
                ProcessMemory::Available { bytes }
            }),
        };
        Ok(ProcessMemoryObservation {
            observed_at: SystemTime::now(),
            memory,
            process: engine,
        })
    }

    /// Identity of the held engine process from the launcher's process record, or `None` while
    /// no record exists.
    async fn host_process(&self, id: &str) -> Option<HostProcessIdentity> {
        let bytes = tokio::fs::read(self.workload_directory(id).join(PROCESS_FILE))
            .await
            .ok()?;
        let record = ProcessRecord::decode(&bytes).ok()?;
        Some(HostProcessIdentity {
            managed_node_id: self.config.site_node_id.clone(),
            process_id: record.pid,
            process_start_token: record.start_token,
        })
    }

    /// Ids of the known workloads that carry exactly this correlation.
    pub(super) fn known_workload_ids(&self, correlation: &WorkloadCorrelation) -> Vec<WorkloadId> {
        self.known
            .lock()
            .expect("native workload registry")
            .values()
            .filter(|known| known.identity.correlation == *correlation)
            .map(|known| known.identity.workload_id.clone())
            .collect()
    }

    fn known_workload(&self, id: &str) -> Option<KnownWorkload> {
        self.known
            .lock()
            .expect("native workload registry")
            .get(id)
            .cloned()
    }

    fn forget(&self, id: &str) {
        self.known
            .lock()
            .expect("native workload registry")
            .remove(id);
    }

    /// Lifetime every new workload is launched with: persistent, or watching the server
    /// process that opened this backend.
    pub fn lifetime(&self) -> Lifetime {
        match self.config.lifetime {
            NativeWorkloadLifetime::Persistent => Lifetime::Independent,
            NativeWorkloadLifetime::Ephemeral => self.supervisor.watch_this_process(),
        }
    }

    pub(super) fn workload_directory(&self, id: &str) -> PathBuf {
        self.state_root.target_directory.join(id)
    }

    /// Error for a workload whose records cannot be trusted, naming its directory.
    fn untrusted(&self, id: &str, error: NativeOwnershipError) -> RuntimeBackendError {
        untrusted_directory(&self.workload_directory(id), error)
    }
}

impl RuntimeBackend for NativeRuntimeBackend {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(BACKEND_ID.to_owned())
    }

    fn execution_platform<'a>(&'a self) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(host_platform()))
    }

    fn observe_process_memory<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, ProcessMemoryObservation> {
        Box::pin(self.process_memory(&workload_id.0))
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        Box::pin(self.discover())
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.create(workload))
    }

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.start(&workload_id.0))
    }

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        Box::pin(self.inspect(&workload_id.0))
    }

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(self.stop(&workload_id.0))
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        Box::pin(self.remove(&workload_id.0))
    }

    fn read_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(self.read_logs(&workload_id.0, query))
    }

    fn follow_workload_logs<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(self.follow_logs(&workload_id.0, query))
    }

    fn read_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(self.read_logs_by_correlation(correlation, query))
    }

    fn follow_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(self.follow_logs_by_correlation(correlation, query))
    }
}

/// Process facts of one known native workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeWorkloadProcess {
    /// Fingerprint of the exact launch intent its owner record holds.
    pub launch_fingerprint: String,

    /// Host process id while the workload runs.
    pub process_id: Option<u32>,

    /// Executable and arguments it runs, with its assigned loopback ports.
    pub command: Vec<String>,

    pub loopback_endpoints: Vec<LoopbackEndpoint>,
}

/// Owned workload of this backend with the identity it reports.
#[derive(Clone, Debug)]
struct KnownWorkload {
    identity: ObservedWorkload,
    record: NativeOwnerRecord,
    /// Owned, but its identity no longer recomputes the same way.
    stale: bool,
}

impl KnownWorkload {
    /// Identity of the running workload with one loopback endpoint and service binding per
    /// declared port; the binding revision is the assigned port, so a reassigned port is a new
    /// binding.
    fn serving_identity(&self) -> ObservedWorkload {
        let mut identity = self.identity.clone();
        identity.loopback_endpoints = self
            .record
            .launch
            .spec
            .ports
            .iter()
            .filter_map(|port| {
                self.record
                    .ports
                    .get(&port.name)
                    .map(|host_port| LoopbackEndpoint {
                        port_name: port.name.clone(),
                        host_port: *host_port,
                    })
            })
            .collect();
        identity.services = identity
            .loopback_endpoints
            .iter()
            .map(|endpoint| {
                WorkloadServiceBinding::loopback(
                    identity.workload_id.0.clone(),
                    endpoint.port_name.clone(),
                    u64::from(endpoint.host_port),
                    endpoint.clone(),
                )
            })
            .collect();
        identity
    }

    fn new(id: &str, record: NativeOwnerRecord, stale: bool) -> Self {
        let recorded = &record.launch.correlation;
        let correlation = WorkloadCorrelation {
            owner_id: recorded.owner_id.clone(),
            // The owner replaces a workload whose generation differs from the one it launches;
            // the complement never equals the recorded generation, nor, short of a hash
            // collision, the one any current launch derives.
            reconcile_generation: if stale {
                (!recorded.reconcile_generation).max(1)
            } else {
                recorded.reconcile_generation
            },
        };
        Self {
            identity: ObservedWorkload {
                workload_id: WorkloadId(id.to_owned()),
                correlation,
                backend_id: RuntimeBackendId(BACKEND_ID.to_owned()),
                loopback_endpoints: Vec::new(),
                services: Vec::new(),
                host_process: None,
            },
            record,
            stale,
        }
    }
}

/// Whether some process accepts connections on a loopback port. Connecting, unlike binding,
/// never takes the port away from the process that is about to listen on it.
async fn accepts_connections(port: u16) -> bool {
    tokio::time::timeout(
        PORT_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)),
    )
    .await
    .is_ok_and(|connected| connected.is_ok())
}

/// Platform of the host this process runs on, where native workloads run too.
pub(super) fn host_platform() -> Result<ExecutionPlatform, RuntimeBackendError> {
    // Rust names the darwin operating system `macos`.
    let operating_system = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    ExecutionPlatform::normalize(operating_system, std::env::consts::ARCH)
        .map_err(RuntimeBackendError::ExecutionPlatform)
}

/// Stops one workload of the target directory when its launcher still runs, then removes its
/// directory, whatever state it ended in; a directory that is already gone counts as removed.
pub(super) async fn stop_and_remove(
    supervisor: &Supervisor,
    target_directory: &Path,
    id: &str,
) -> Result<(), RuntimeBackendError> {
    let state = match supervisor.inspect(id).await {
        Ok(WorkloadState::Running { .. } | WorkloadState::Terminating) => {
            match tolerate_gone(supervisor.stop(id).await)
                .map_err(|error| supervisor_error(format!("stop native workload {id}"), error))?
            {
                Some(state) => state,
                None => return Ok(()),
            }
        }
        Ok(state) => state,
        Err(SupervisorError::NotFound { .. }) => return Ok(()),
        Err(error) => {
            return Err(supervisor_error(
                format!("inspect native workload {id}"),
                error,
            ));
        }
    };
    let removed = match state {
        WorkloadState::Interrupted => supervisor.remove_interrupted(id).await,
        WorkloadState::Inconsistent { problem } => {
            return Err(untrusted_directory(
                &target_directory.join(id),
                NativeOwnershipError::Untrusted(problem),
            ));
        }
        _ => supervisor.remove(id).await,
    };
    tolerate_gone(removed)
        .map(|_| ())
        .map_err(|error| supervisor_error(format!("remove native workload {id}"), error))
}

fn untrusted_directory(directory: &Path, error: NativeOwnershipError) -> RuntimeBackendError {
    RuntimeBackendError::Ownership(anyhow::Error::new(error).context(format!(
        "native workload directory {} cannot be trusted; Dotcore never adopts, stops, or removes \
         it, so stop its processes and remove the directory by hand",
        directory.display()
    )))
}

pub(super) fn supervisor_error(
    context: impl Into<String>,
    error: SupervisorError,
) -> RuntimeBackendError {
    RuntimeBackendError::native_with_source(context, error)
}

/// Treats a workload directory that is already gone, for example removed by an overlapping
/// discovery, as done.
fn tolerate_gone<T>(result: Result<T, SupervisorError>) -> Result<Option<T>, SupervisorError> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(SupervisorError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

fn unknown_workload(id: &str) -> RuntimeBackendError {
    RuntimeBackendError::invalid_launch(format!("native workload {id} is unknown"))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use dotcore_domain::test_support::ports::reserve_port;
    use dotcore_launcher::workload::{
        LAUNCH_FILE, LAUNCHER_FILE, LauncherRecord, PROCESS_FILE, ProcessRecord, StoredRecord,
    };
    use dotcore_runtime::{
        RuntimeLogEvent, RuntimeLogFollowQuery, RuntimeLogQuery, RuntimeLogSource,
        RuntimeLogStreamKind, WorkloadValue,
    };
    use futures_util::StreamExt as _;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::native::{
        NativeStateRoot,
        launch::tests::Fixture,
        test_support::{launcher_binary, wait_until},
    };

    /// Fake engine that writes four lines alternating between its output streams, spaced apart so
    /// their timestamps differ, then runs until stopped.
    const LOGGING: &str = "trap 'exit 0' TERM\necho one\nsleep 0.05\necho two >&2\n\
         sleep 0.05\necho three\nsleep 0.05\necho four >&2\n\
         while :; do sleep 0.05; done";

    /// Fake engine that reports readiness once it handles SIGTERM, then runs until stopped and
    /// exits 0.
    const SERVE: &str = "trap 'exit 0' TERM\necho ready\nwhile :; do sleep 0.05; done";

    #[tokio::test]
    async fn workloads_are_created_inspected_stopped_restarted_and_removed() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));

        let created = native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let first_pid = native.engine_pid(&id.0).await;
        native.wait_ready(&id.0).await;
        let first_ports = native.recorded_ports(&id.0);
        let first_identity = native.serving_identity(&launch, &first_ports);
        native.backend.stop_workload(&id).await.expect("stop");
        let stopped = native
            .backend
            .inspect_workload(&id)
            .await
            .expect("inspect stopped");
        let restarted = native.backend.start_workload(&id).await.expect("restart");
        let second_pid = native.engine_pid(&id.0).await;
        let restarted_ports = native.recorded_ports(&id.0);
        let restarted_identity = native.serving_identity(&launch, &restarted_ports);
        native.backend.remove_workload(&id).await.expect("remove");

        assert_eq!(created, WorkloadObservation::Running(first_identity));
        assert_eq!(
            stopped,
            WorkloadObservation::Stopped {
                workload: expected_identity(&launch),
                exit_status: Some(0),
            }
        );
        assert_eq!(restarted, WorkloadObservation::Running(restarted_identity));
        assert_ne!(first_pid, second_pid);
        assert_eq!(restarted_ports, first_ports);
        assert!(!native.directory(&id.0).exists());
        assert!(native.backend.inspect_workload(&id).await.is_err());
    }

    #[tokio::test]
    async fn process_facts_name_the_recorded_launch_its_command_and_its_running_process() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        assert_eq!(
            native
                .backend
                .workload_directory_count()
                .await
                .expect("count"),
            0
        );

        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let pid = native.engine_pid(&id.0).await;
        let ports = native.recorded_ports(&id.0);
        let running = native
            .backend
            .workload_process(&id)
            .await
            .expect("process facts");
        let expected_endpoints = native.serving_identity(&launch, &ports).loopback_endpoints;
        native.backend.stop_workload(&id).await.expect("stop");
        let stopped = native
            .backend
            .workload_process(&id)
            .await
            .expect("stopped facts");
        let count = native
            .backend
            .workload_directory_count()
            .await
            .expect("count");
        native.backend.remove_workload(&id).await.expect("remove");

        assert_eq!(
            running.launch_fingerprint,
            launch.exact_intent_fingerprint()
        );
        assert_eq!(running.process_id, Some(pid));
        assert_eq!(running.loopback_endpoints, expected_endpoints);
        let port = ports.get("http").expect("assigned port");
        assert!(
            running
                .command
                .first()
                .is_some_and(|executable| executable.ends_with("/bin/server"))
                && running.command.contains(&format!("--port={port}")),
            "{:?}",
            running.command
        );
        assert_eq!(stopped.process_id, None);
        assert_eq!(stopped.launch_fingerprint, running.launch_fingerprint);
        assert_eq!(count, 1);
        assert_eq!(
            native
                .backend
                .workload_directory_count()
                .await
                .expect("count"),
            0
        );
    }

    #[tokio::test]
    async fn an_exit_while_no_backend_ran_is_reported_with_its_status() {
        let native = Native::new("sleep 0.3\nexit 7");
        let launch = native.fixture.launch();
        let id = workload_id(&launch.correlation);
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let directory = native.directory(&id);
        let Native { fixture, backend } = native;
        drop(backend);

        wait_until(|| directory.join("exit.pb").exists()).await;
        let restarted = backend_for(&fixture);
        let discovered = restarted.discover_workloads().await.expect("discover");

        assert_eq!(
            discovered,
            [WorkloadObservation::Stopped {
                workload: expected_identity(&launch),
                exit_status: Some(7),
            }]
        );
        restarted
            .remove_workload(&WorkloadId(id))
            .await
            .expect("remove");
    }

    #[tokio::test]
    async fn a_new_backend_adopts_the_running_workload_with_the_same_identity_and_process() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = workload_id(&launch.correlation);
        let created = native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let pid = native.engine_pid(&id).await;
        let Native { fixture, backend } = native;
        drop(backend);

        let native = Native {
            backend: backend_for(&fixture),
            fixture,
        };
        let discovered = native.backend.discover_workloads().await.expect("discover");

        assert_eq!(discovered, [created]);
        assert_eq!(native.engine_pid(&id).await, pid);
        native
            .backend
            .remove_workload(&WorkloadId(id))
            .await
            .expect("remove");
    }

    #[tokio::test]
    async fn discovery_adopts_the_same_intent_and_reports_changed_intent_for_replacement() {
        let native = Native::new(SERVE);
        let intent_a = native.fixture.launch();
        native
            .backend
            .create_workload(&intent_a)
            .await
            .expect("create A");
        let mut changed_argv = intent_a.spec.clone();
        changed_argv
            .arguments
            .push(WorkloadValue::literal("--verbose"));
        let mut changed_executable = intent_a.spec.clone();
        changed_executable.executable.path = "bin/other".to_owned();

        let discovered = native
            .fixture_backend()
            .discover_workloads()
            .await
            .expect("discover");

        let [WorkloadObservation::Running(observed)] = discovered.as_slice() else {
            panic!("expected one running workload, got {discovered:?}");
        };
        assert_eq!(observed.correlation, intent_a.correlation);
        for intent_b in [changed_argv, changed_executable] {
            let intent_b = WorkloadLaunch::new(
                intent_a.correlation.owner_id.clone(),
                intent_a.execution_target.clone(),
                intent_b,
            );
            assert_ne!(
                observed.correlation.reconcile_generation,
                intent_b.correlation.reconcile_generation
            );
        }
        native
            .backend
            .remove_workload(&observed.workload_id)
            .await
            .expect("remove");
    }

    #[tokio::test]
    async fn stale_workloads_are_reported_for_replacement_and_stay_removable() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let mut prepared = native.fixture.prepare(&launch).expect("prepare");
        prepared.record.identity_version -= 1;
        prepared.spec.owner = prepared.record.encode().expect("encode stale record");
        prepared.spec.lifetime = native.supervisor().watch_this_process();
        native
            .supervisor()
            .create(&prepared.workload_id, &prepared.spec)
            .await
            .expect("create stale workload");

        let discovered = native.backend.discover_workloads().await.expect("discover");

        let [WorkloadObservation::Running(observed)] = discovered.as_slice() else {
            panic!("expected one running workload, got {discovered:?}");
        };
        assert_eq!(observed.workload_id.0, prepared.workload_id);
        assert_ne!(
            observed.correlation.reconcile_generation,
            launch.correlation.reconcile_generation
        );
        assert_ne!(observed.correlation.reconcile_generation, 0);
        assert!(
            native
                .backend
                .start_workload(&observed.workload_id)
                .await
                .is_err()
        );
        native
            .backend
            .stop_workload(&observed.workload_id)
            .await
            .expect("stop");
        native
            .backend
            .remove_workload(&observed.workload_id)
            .await
            .expect("remove");
        assert!(!native.directory(&prepared.workload_id).exists());
    }

    #[tokio::test]
    async fn a_tampered_launch_record_fails_discovery_and_blocks_mutation() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let record = native.directory(&id.0).join(LAUNCH_FILE);
        let original = fs::read(&record).unwrap();
        let mut tampered = original.clone();
        tampered.push(0);
        replace_file(&record, &tampered);

        let discovery = native.fixture_backend().discover_workloads().await;
        let stop = native.backend.stop_workload(&id).await;
        let remove = native.backend.remove_workload(&id).await;
        let inspect_after_tamper = native.backend.inspect_workload(&id).await;

        let message = format!(
            "{:#}",
            anyhow::Error::new(discovery.expect_err("discovery fails"))
        );
        assert!(
            message.contains(&native.directory(&id.0).display().to_string()),
            "{message}"
        );
        assert!(
            matches!(stop, Err(RuntimeBackendError::Ownership(_))),
            "{stop:?}"
        );
        assert!(
            matches!(remove, Err(RuntimeBackendError::Ownership(_))),
            "{remove:?}"
        );
        assert!(inspect_after_tamper.is_err());
        assert!(native.engine_pid(&id.0).await > 0);
        replace_file(&record, &original);
        native
            .backend
            .remove_workload(&id)
            .await
            .expect("remove restored");
    }

    #[tokio::test]
    async fn a_workload_from_another_boot_is_absent_and_its_directory_removed() {
        let native = Native::new("exit 0");
        let launch = native.fixture.launch();
        let id = workload_id(&launch.correlation);
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let directory = native.directory(&id);
        wait_until(|| directory.join("exit.pb").exists()).await;
        let launcher_file = directory.join(LAUNCHER_FILE);
        let mut record = LauncherRecord::decode(&fs::read(&launcher_file).unwrap()).unwrap();
        record.boot_id = "an-earlier-boot".to_owned();
        replace_file(&launcher_file, &record.encode());

        let discovered = native
            .fixture_backend()
            .discover_workloads()
            .await
            .expect("discover");

        assert_eq!(
            discovered,
            [WorkloadObservation::Absent(expected_identity(&launch))]
        );
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn discovery_removes_interrupted_creates() {
        let native = Native::new(SERVE);
        let directory = native.directory("deployment-one_5");
        fs::create_dir(&directory).unwrap();
        fs::File::open(&directory)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();

        let discovered = native.backend.discover_workloads().await.expect("discover");

        assert_eq!(discovered, []);
        assert!(!directory.exists());
    }

    #[tokio::test]
    async fn a_workload_bound_to_an_earlier_server_is_drained_and_never_adopted() {
        let native = Native::new(SERVE);
        let mut earlier_server = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("earlier server process");
        let pid = earlier_server.id();
        let start_token = dotcore_host_process::start_token(pid)
            .unwrap()
            .expect("earlier server start token");
        let launch = native.fixture.launch();
        let prepared = prepare_launch(
            &native.fixture.config,
            &native.fixture.state_root,
            &launch,
            BTreeMap::from([("http".to_owned(), 41_234)]),
            Lifetime::Watch { pid, start_token },
        )
        .expect("prepare");
        native
            .supervisor()
            .create(&prepared.workload_id, &prepared.spec)
            .await
            .expect("create");
        native.engine_pid(&prepared.workload_id).await;

        let discovered = native.backend.discover_workloads().await;
        earlier_server.kill().unwrap();
        earlier_server.wait().unwrap();

        assert_eq!(
            discovered.expect("discover"),
            [WorkloadObservation::Absent(expected_identity(&launch))]
        );
        assert!(!native.directory(&prepared.workload_id).exists());
    }

    #[tokio::test]
    async fn a_copied_site_directory_neither_adopts_nor_stops_the_original_workloads() {
        let original = Native::new(SERVE);
        let launch = original.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        original
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        original.engine_pid(&id.0).await;
        let copy = tempfile::tempdir().unwrap();
        copy_tree(
            original.fixture.config.state_root.path().parent().unwrap(),
            &copy.path().join("runtime"),
        );
        let copied = NativeRuntimeBackend::new(
            NativeRuntimeConfig {
                state_root: NativeStateRoot::for_site(copy.path()),
                ..original.config()
            },
            launcher_binary(),
        )
        .expect("copied backend")
        .with_port_settle_window(Timing::test().native_port_settle_window);

        let discovered = copied.discover_workloads().await.expect("discover copy");

        assert_eq!(discovered, []);
        assert!(copied.stop_workload(&id).await.is_err());
        assert_eq!(
            original
                .backend
                .inspect_workload(&id)
                .await
                .expect("inspect original"),
            WorkloadObservation::Running(
                original.serving_identity(&launch, &original.recorded_ports(&id.0))
            )
        );
        assert!(
            copy.path()
                .join("runtime/native/mac-gpu")
                .join(&id.0)
                .exists()
        );
        original.backend.remove_workload(&id).await.expect("remove");
    }

    #[tokio::test]
    async fn workloads_get_distinct_loopback_ports_bound_as_services() {
        let native = Native::new(SERVE);
        let first = native.fixture.launch();
        let second = WorkloadLaunch::new(
            "deployment-two",
            first.execution_target.clone(),
            first.spec.clone(),
        );

        let observations = [
            native
                .backend
                .create_workload(&first)
                .await
                .expect("create first"),
            native
                .backend
                .create_workload(&second)
                .await
                .expect("create second"),
        ];

        let mut ports = Vec::new();
        for (launch, observation) in [&first, &second].into_iter().zip(&observations) {
            let id = workload_id(&launch.correlation);
            let recorded = native.recorded_ports(&id);
            assert_eq!(
                *observation,
                WorkloadObservation::Running(native.serving_identity(launch, &recorded))
            );
            ports.push(recorded["http"]);
            native
                .backend
                .remove_workload(&WorkloadId(id))
                .await
                .expect("remove");
        }
        assert_ne!(ports[0], ports[1]);
    }

    #[tokio::test]
    async fn a_restart_reassigns_a_taken_port_without_changing_the_identity() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        // The first assignment is a pool port, so the one taken below can't be an ephemeral port.
        let reserved = reserve_port();
        native
            .backend
            .launch(
                &id.0,
                &launch,
                BTreeMap::from([("http".to_owned(), reserved.port())]),
            )
            .await
            .expect("create");
        native.wait_ready(&id.0).await;
        native.backend.stop_workload(&id).await.expect("stop");
        let previous = native.recorded_ports(&id.0);
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, previous["http"]))
            .expect("take the recorded port");

        let restarted = native.backend.start_workload(&id).await.expect("restart");
        let reassigned = native.recorded_ports(&id.0);
        let reassigned_identity = native.serving_identity(&launch, &reassigned);
        let adopted = native
            .fixture_backend()
            .discover_workloads()
            .await
            .expect("discover");
        drop(taken);
        native.backend.remove_workload(&id).await.expect("remove");

        assert_eq!(previous["http"], reserved.port());
        assert_ne!(reassigned["http"], previous["http"]);
        assert_eq!(restarted, WorkloadObservation::Running(reassigned_identity));
        assert_eq!(adopted, [restarted]);
    }

    #[tokio::test]
    async fn an_exit_on_a_taken_port_is_a_retryable_create_failure() {
        let mut native = Native::new("exit 1");
        // A loaded test host can start the fake engine slower than the production window.
        native.backend.port_settle_window = Duration::from_secs(20);
        let launch = native.fixture.launch();
        let id = workload_id(&launch.correlation);
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("taken port");
        let port = taken.local_addr().unwrap().port();

        let failed = native
            .backend
            .launch(&id, &launch, BTreeMap::from([("http".to_owned(), port)]))
            .await;
        let gone = !native.directory(&id).exists() && native.backend.known_workload(&id).is_none();
        drop(taken);
        let retried = native.backend.create_workload(&launch).await;

        assert!(
            matches!(failed, Err(RuntimeBackendError::Native(_))),
            "{failed:?}"
        );
        assert!(gone);
        assert_eq!(
            retried.expect("retried create"),
            WorkloadObservation::Stopped {
                workload: expected_identity(&launch),
                exit_status: Some(1),
            }
        );
        native
            .backend
            .remove_workload(&WorkloadId(id))
            .await
            .expect("remove");
    }

    #[tokio::test]
    async fn the_service_binding_reaches_a_fake_engine_on_its_loopback_port() {
        let executable = std::env::current_exe().expect("test executable");
        let native = Native::new(&format!(
            "exec '{}' --exact native::adapter::tests::fake_http_engine --nocapture",
            executable.display()
        ));
        let mut spec = native.fixture.launch().spec;
        spec.environment.insert(
            FAKE_ENGINE_PORT.to_owned(),
            WorkloadValue::parse("{service:http.port}").unwrap(),
        );
        spec.environment.insert(
            FAKE_ENGINE_ALIAS.to_owned(),
            WorkloadValue::literal("fake-model"),
        );
        let launch = WorkloadLaunch::new(
            "deployment-one",
            native.fixture.launch().execution_target,
            spec,
        );

        let created = native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let WorkloadObservation::Running(identity) = &created else {
            panic!("expected a running workload, got {created:?}");
        };
        let [service] = identity.services.as_slice() else {
            panic!("expected one service binding, got {:?}", identity.services);
        };
        let dotcore_runtime::WorkloadServiceAccess::Loopback(endpoint) = &service.access else {
            panic!("expected a loopback service binding");
        };
        let deadline = Instant::now() + Duration::from_secs(20);
        let models = loop {
            match loopback_get(endpoint.host_port, "/v1/models") {
                Ok(body) => break Ok(body),
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Err(error) => break Err(error),
            }
        };
        native
            .backend
            .remove_workload(&identity.workload_id)
            .await
            .expect("remove");

        assert_eq!(service.reference.revision, u64::from(endpoint.host_port));
        assert_eq!(service.reference.service_name, "http");
        assert_eq!(
            models.expect("fake engine reached through the service binding"),
            r#"{"object":"list","data":[{"id":"fake-model"}]}"#
        );
    }

    #[tokio::test]
    async fn the_running_workload_reports_the_held_engine_process() {
        let native = Native::new(SERVE);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));

        let created = native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let pid = native.engine_pid(&id.0).await;
        let live_token = dotcore_host_process::read_process_start_token(pid).expect("live engine");
        let adopted = native.fixture_backend().discover_workloads().await;
        native.backend.stop_workload(&id).await.expect("stop");
        let stopped = native.backend.inspect_workload(&id).await.expect("inspect");
        native.backend.remove_workload(&id).await.expect("remove");

        let expected = Some(HostProcessIdentity {
            managed_node_id: native.fixture.config.site_node_id.clone(),
            process_id: pid,
            process_start_token: live_token,
        });
        let WorkloadObservation::Running(running) = created else {
            panic!("expected a running workload, got {created:?}");
        };
        assert_eq!(running.host_process, expected);
        assert_eq!(
            adopted.expect("discover"),
            [WorkloadObservation::Running(running)]
        );
        assert!(matches!(
            stopped,
            WorkloadObservation::Stopped { workload, .. } if workload.host_process.is_none()
        ));
    }

    #[tokio::test]
    async fn logs_are_read_by_id_and_correlation_with_before_and_limit_paging() {
        let native = Native::new(LOGGING);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        native.wait_logged(&id.0, "four").await;

        let all = native
            .backend
            .read_workload_logs(&id, &log_query(200, None))
            .await
            .expect("read all");
        let by_correlation = native
            .backend
            .read_workload_logs_by_correlation(&launch.correlation, &log_query(200, None))
            .await
            .expect("read by correlation");
        let newest = native
            .backend
            .read_workload_logs(&id, &log_query(2, None))
            .await
            .expect("read newest");
        let boundary = all.entries[2].timestamp;
        let older = native
            .backend
            .read_workload_logs(&id, &log_query(200, Some(boundary)))
            .await
            .expect("read before");
        let older_limited = native
            .backend
            .read_workload_logs(&id, &log_query(1, Some(boundary)))
            .await
            .expect("read before limited");
        let unknown = native
            .backend
            .read_workload_logs_by_correlation(
                &WorkloadCorrelation {
                    owner_id: "other".to_owned(),
                    reconcile_generation: 1,
                },
                &log_query(200, None),
            )
            .await;
        native.backend.remove_workload(&id).await.expect("remove");

        assert_eq!(
            lines(&all),
            [
                ("one", RuntimeLogStreamKind::Stdout),
                ("two", RuntimeLogStreamKind::Stderr),
                ("three", RuntimeLogStreamKind::Stdout),
                ("four", RuntimeLogStreamKind::Stderr),
            ]
        );
        assert!(!all.has_older_output);
        assert!(
            all.entries
                .windows(2)
                .all(|pair| pair[0].timestamp < pair[1].timestamp)
        );
        assert!(all.entries.iter().all(|entry| {
            entry.source == log_source() && !entry.truncated && entry.timestamp > UNIX_EPOCH
        }));
        assert_eq!(by_correlation, all);
        assert_eq!(newest.entries, all.entries[2..]);
        assert!(newest.has_older_output);
        assert_eq!(older.entries, all.entries[..2]);
        assert!(!older.has_older_output);
        assert_eq!(older_limited.entries, all.entries[1..2]);
        assert!(older_limited.has_older_output);
        assert!(matches!(
            unknown,
            Err(RuntimeBackendError::WorkloadNotCreated)
        ));
    }

    #[tokio::test]
    async fn followed_logs_replay_after_since_and_end_when_the_workload_is_removed() {
        let native = Native::new(LOGGING);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        native.wait_logged(&id.0, "four").await;
        let all = native
            .backend
            .read_workload_logs(&id, &log_query(200, None))
            .await
            .expect("read all");

        let mut replay = native
            .backend
            .follow_workload_logs(&id, follow_query(None, CancellationToken::new()))
            .await
            .expect("follow");
        let mut resumed = native
            .backend
            .follow_workload_logs_by_correlation(
                &launch.correlation,
                follow_query(Some(all.entries[1].timestamp), CancellationToken::new()),
            )
            .await
            .expect("follow by correlation");
        let mut replayed = Vec::new();
        for _ in 0..all.entries.len() {
            replayed.push(next_event(&mut replay).await);
        }
        let resumed_first = next_event(&mut resumed).await;
        let resumed_second = next_event(&mut resumed).await;
        native.backend.remove_workload(&id).await.expect("remove");
        let replay_end = drain_to_end(&mut replay).await;
        let resumed_end = drain_to_end(&mut resumed).await;

        assert_eq!(
            replayed,
            all.entries
                .iter()
                .cloned()
                .map(RuntimeLogEvent::Entry)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            resumed_first,
            RuntimeLogEvent::Entry(all.entries[2].clone())
        );
        assert_eq!(
            resumed_second,
            RuntimeLogEvent::Entry(all.entries[3].clone())
        );
        assert_eq!(replay_end, [], "a removed workload's source ends");
        assert_eq!(resumed_end, []);
    }

    #[tokio::test]
    async fn followed_logs_end_when_the_process_exits_and_replay_a_relaunch_from_its_first_line() {
        let native = Native::new(LOGGING);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let mut first_run = native
            .backend
            .follow_workload_logs(&id, follow_query(None, CancellationToken::new()))
            .await
            .expect("follow");
        let mut first_lines = Vec::new();
        for _ in 0..4 {
            first_lines.push(next_event(&mut first_run).await);
        }

        native.backend.stop_workload(&id).await.expect("stop");
        let first_end = drain_to_end(&mut first_run).await;
        native.backend.start_workload(&id).await.expect("relaunch");
        native.wait_logged(&id.0, "four").await;
        let last_delivered = match &first_lines[3] {
            RuntimeLogEvent::Entry(entry) => entry.timestamp,
            event => panic!("expected an entry, got {event:?}"),
        };
        let mut resumed = native
            .backend
            .follow_workload_logs(
                &id,
                follow_query(Some(last_delivered), CancellationToken::new()),
            )
            .await
            .expect("follow after the last delivered line");
        let mut relaunched = native
            .backend
            .follow_workload_logs(&id, follow_query(None, CancellationToken::new()))
            .await
            .expect("follow from the first line");
        let mut resumed_lines = Vec::new();
        let mut relaunched_lines = Vec::new();
        for _ in 0..4 {
            resumed_lines.push(next_event(&mut resumed).await);
            relaunched_lines.push(next_event(&mut relaunched).await);
        }
        native.backend.remove_workload(&id).await.expect("remove");

        let texts = |events: &[RuntimeLogEvent]| {
            events
                .iter()
                .map(|event| match event {
                    RuntimeLogEvent::Entry(entry) => {
                        String::from_utf8_lossy(&entry.content).into_owned()
                    }
                    event => panic!("expected an entry, got {event:?}"),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&first_lines), ["one", "two", "three", "four"]);
        assert_eq!(first_end, [], "an exited process's source ends");
        assert_eq!(
            texts(&relaunched_lines),
            ["one", "two", "three", "four"],
            "the relaunch replays from its own first line"
        );
        assert_eq!(resumed_lines, relaunched_lines);
        assert!(
            resumed_lines.iter().all(|event| matches!(
                event,
                RuntimeLogEvent::Entry(entry) if entry.timestamp > last_delivered
            )),
            "the relaunch's lines all follow the earlier run's"
        );
    }

    #[tokio::test]
    async fn followed_logs_stop_on_cancellation_and_reject_a_cancelled_or_removed_workload() {
        let native = Native::new(LOGGING);
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        native.wait_logged(&id.0, "four").await;

        let cancellation = CancellationToken::new();
        let mut stream = native
            .backend
            .follow_workload_logs(&id, follow_query(None, cancellation.clone()))
            .await
            .expect("follow");
        for _ in 0..4 {
            next_event(&mut stream).await;
        }
        cancellation.cancel();
        let ended = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("stream closes");
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        let rejected = native
            .backend
            .follow_workload_logs(&id, follow_query(None, cancelled))
            .await;
        native.backend.remove_workload(&id).await.expect("remove");
        let removed = native
            .backend
            .follow_workload_logs(&id, follow_query(None, CancellationToken::new()))
            .await;

        assert!(ended.is_none());
        assert!(matches!(rejected, Err(RuntimeBackendError::LogCancelled)));
        assert!(removed.is_err());
    }

    #[tokio::test]
    async fn process_memory_counts_the_engine_and_its_child_but_only_while_running() {
        const BUFFER_BYTES: u64 = 64 * 1024 * 1024;
        let executable = std::env::current_exe().expect("test executable");
        let native = Native::new(&format!(
            "exec '{}' --exact native::adapter::tests::fake_memory_engine --nocapture",
            executable.display()
        ));
        let mut spec = native.fixture.launch().spec;
        spec.environment.insert(
            FAKE_MEMORY_BYTES.to_owned(),
            WorkloadValue::literal(BUFFER_BYTES.to_string()),
        );
        spec.environment
            .insert(FAKE_MEMORY_CHILD.to_owned(), WorkloadValue::literal("1"));
        let launch = WorkloadLaunch::new(
            "deployment-one",
            native.fixture.launch().execution_target,
            spec,
        );
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let log = native.directory(&id.0).join("stdout.log");
        wait_until(|| {
            fs::read_to_string(&log).is_ok_and(|text| text.matches(" allocated").count() == 2)
        })
        .await;
        let engine = native.engine_pid(&id.0).await;
        let engine_alone = dotcore_host_process::memory_bytes(engine)
            .unwrap()
            .expect("engine memory");

        let running = native
            .backend
            .observe_process_memory(&id)
            .await
            .expect("observe running");
        native.backend.stop_workload(&id).await.expect("stop");
        let stopped = native
            .backend
            .observe_process_memory(&id)
            .await
            .expect("observe stopped");
        native.backend.remove_workload(&id).await.expect("remove");

        let ProcessMemory::Available { bytes } = running.memory else {
            panic!("expected available memory, got {:?}", running.memory);
        };
        assert!(bytes >= 2 * BUFFER_BYTES, "{bytes}");
        assert!(
            bytes >= engine_alone + BUFFER_BYTES,
            "{bytes} vs engine {engine_alone}"
        );
        assert_eq!(
            running.process.map(|process| process.process_id),
            Some(engine)
        );
        assert!(matches!(
            stopped.memory,
            ProcessMemory::Unavailable(ProcessMemoryUnavailable::NotRunning)
        ));
        assert_eq!(stopped.process, None);
    }

    #[tokio::test]
    async fn a_restart_during_a_recent_create_reports_nothing_and_leaves_it_alone() {
        let native = Native::new(SERVE);
        let published = native.directory("deployment-one_7");
        fs::create_dir(&published).unwrap();
        fs::write(published.join(LAUNCH_FILE), b"partial").unwrap();
        let staging = native
            .fixture
            .state_root
            .target_directory
            .join(".create-deployment-two_8");
        fs::create_dir(&staging).unwrap();

        let discovered = native.fixture_backend().discover_workloads().await;

        assert_eq!(discovered.expect("discover"), []);
        assert!(published.exists());
        assert!(staging.exists());
    }

    #[tokio::test]
    async fn a_relaunch_in_flight_is_starting_and_never_reported_untrusted() {
        let native = Native::new("exit 0");
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let directory = native.directory(&id.0);
        wait_until(|| directory.join("exit.pb").exists()).await;
        // What a relaunch leaves between removing the finished workload and publishing the new one.
        fs::remove_dir_all(&directory).unwrap();
        fs::create_dir(&directory).unwrap();

        let inspected = native.backend.inspect_workload(&id).await;
        let memory = native.backend.observe_process_memory(&id).await;
        let stopped = native.backend.stop_workload(&id).await;

        assert_eq!(
            inspected.expect("inspect"),
            WorkloadObservation::Starting(expected_identity(&launch))
        );
        assert!(matches!(
            memory.expect("memory").memory,
            ProcessMemory::Unavailable(ProcessMemoryUnavailable::NotRunning)
        ));
        assert!(stopped.is_ok(), "{stopped:?}");
        fs::remove_dir_all(&directory).unwrap();
    }

    #[tokio::test]
    async fn a_relaunch_that_cannot_start_keeps_the_finished_workload() {
        let native = Native::new("exit 3");
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));
        native
            .backend
            .create_workload(&launch)
            .await
            .expect("create");
        let directory = native.directory(&id.0);
        wait_until(|| directory.join("exit.pb").exists()).await;
        fs::remove_file(PathBuf::from(&native.fixture.store_path).join("bin/server")).unwrap();

        let started = native.backend.start_workload(&id).await;

        assert!(
            matches!(started, Err(RuntimeBackendError::InvalidLaunch(_))),
            "{started:?}"
        );
        assert_eq!(
            native.backend.inspect_workload(&id).await.expect("inspect"),
            WorkloadObservation::Stopped {
                workload: expected_identity(&launch),
                exit_status: Some(3),
            }
        );
        native.backend.remove_workload(&id).await.expect("remove");
    }

    #[tokio::test]
    async fn overlapping_discoveries_and_creates_keep_every_workload_known() {
        let native = Native::new(SERVE);
        let interrupted = native.directory("deployment-two_5");
        fs::create_dir(&interrupted).unwrap();
        fs::File::open(&interrupted)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(600))
            .unwrap();
        let launch = native.fixture.launch();
        let id = WorkloadId(workload_id(&launch.correlation));

        let (first, second, created) = tokio::join!(
            native.backend.discover_workloads(),
            native.backend.discover_workloads(),
            native.backend.create_workload(&launch),
        );
        let after = native.backend.discover_workloads().await;

        first.expect("first discovery");
        second.expect("second discovery");
        created.expect("create");
        assert!(!interrupted.exists());
        assert!(matches!(
            native.backend.inspect_workload(&id).await,
            Ok(WorkloadObservation::Running(_))
        ));
        assert_eq!(after.expect("discovery after").len(), 1);
        native.backend.remove_workload(&id).await.expect("remove");
    }

    #[tokio::test]
    async fn process_memory_of_an_unknown_workload_is_not_running() {
        let native = Native::new(SERVE);

        let observation = native
            .backend
            .observe_process_memory(&WorkloadId("deployment-one_1".to_owned()))
            .await
            .expect("observe");

        assert!(matches!(
            observation.memory,
            ProcessMemory::Unavailable(ProcessMemoryUnavailable::NotRunning)
        ));
    }

    /// Allocates and touches a buffer, optionally in a child process too, then idles, when the
    /// test binary runs as a native workload with the fake memory environment; otherwise
    /// returns at once.
    #[test]
    fn fake_memory_engine() {
        let Ok(bytes) = std::env::var(FAKE_MEMORY_BYTES) else {
            return;
        };
        let buffer = vec![1_u8; bytes.parse().expect("buffer size")];
        let _child = std::env::var(FAKE_MEMORY_CHILD).is_ok().then(|| {
            std::process::Command::new(std::env::current_exe().expect("test executable"))
                .args(["--exact", "native::adapter::tests::fake_memory_engine"])
                .arg("--nocapture")
                .env_remove(FAKE_MEMORY_CHILD)
                .spawn()
                .expect("fake engine child")
        });
        println!("\nallocated {}", std::hint::black_box(&buffer).len());
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Environment variable carrying the buffer size the fake memory engine allocates.
    const FAKE_MEMORY_BYTES: &str = "DOTCORE_FAKE_MEMORY_BYTES";

    /// Environment variable asking the fake memory engine to start one allocating child.
    const FAKE_MEMORY_CHILD: &str = "DOTCORE_FAKE_MEMORY_CHILD";

    /// Serves the fake engine's readiness endpoints when the test binary runs as a native
    /// workload with the fake engine environment; otherwise returns at once.
    #[test]
    fn fake_http_engine() {
        let Ok(port) = std::env::var(FAKE_ENGINE_PORT) else {
            return;
        };
        let alias = std::env::var(FAKE_ENGINE_ALIAS).expect("fake engine alias");
        let listener =
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port.parse::<u16>().unwrap()))
                .expect("fake engine port");
        for stream in listener.incoming() {
            let mut stream = stream.expect("fake engine connection");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = std::io::Read::read(&mut stream, &mut buffer).expect("request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request);
            let body = match request.split_whitespace().nth(1) {
                Some("/health") => r#"{"status":"ok"}"#.to_owned(),
                Some("/v1/models") => format!(r#"{{"object":"list","data":[{{"id":"{alias}"}}]}}"#),
                _ => String::new(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            std::io::Write::write_all(&mut stream, response.as_bytes()).expect("response");
        }
    }

    /// Environment variable carrying the fake engine's assigned port.
    const FAKE_ENGINE_PORT: &str = "DOTCORE_FAKE_ENGINE_PORT";

    /// Environment variable carrying the model alias the fake engine serves.
    const FAKE_ENGINE_ALIAS: &str = "DOTCORE_FAKE_ENGINE_ALIAS";

    /// Sends one HTTP GET for `path` to the loopback `port` and returns the response body.
    fn loopback_get(port: u16, path: &str) -> std::io::Result<String> {
        let mut stream = std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        std::io::Write::write_all(
            &mut stream,
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )?;
        let mut response = String::new();
        std::io::Read::read_to_string(&mut stream, &mut response)?;
        response
            .split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .ok_or_else(|| std::io::Error::other("incomplete HTTP response"))
    }

    /// Native backend over a fixture whose fake engine runs `script`; workloads stop once the
    /// test process exits.
    struct Native {
        fixture: Fixture,
        backend: NativeRuntimeBackend,
    }

    impl Native {
        fn new(script: &str) -> Self {
            let fixture = Fixture::new();
            let engine = PathBuf::from(&fixture.store_path).join("bin/server");
            // The launcher clears the environment, and hosts like NixOS have no `/bin/sleep`, so the
            // script carries this test's `PATH` to find its commands.
            let path = std::env::var("PATH").unwrap();
            fs::write(&engine, format!("#!/bin/sh\nPATH='{path}'\n{script}\n")).unwrap();
            fs::set_permissions(&engine, fs::Permissions::from_mode(0o755)).unwrap();
            fs::copy(&engine, engine.with_file_name("other")).unwrap();
            Self {
                backend: backend_for(&fixture),
                fixture,
            }
        }

        fn config(&self) -> NativeRuntimeConfig {
            NativeRuntimeConfig {
                lifetime: NativeWorkloadLifetime::Ephemeral,
                ..self.fixture.config.clone()
            }
        }

        fn fixture_backend(&self) -> NativeRuntimeBackend {
            backend_for(&self.fixture)
        }

        fn supervisor(&self) -> Supervisor {
            Supervisor::new(&self.fixture.state_root.target_directory, launcher_binary())
                .expect("supervisor")
        }

        fn directory(&self, id: &str) -> PathBuf {
            self.fixture.state_root.target_directory.join(id)
        }

        /// Waits for the held engine process and returns its PID.
        async fn engine_pid(&self, id: &str) -> u32 {
            let file = self.directory(id).join(PROCESS_FILE);
            wait_until(|| file.exists()).await;
            ProcessRecord::decode(&fs::read(&file).unwrap())
                .unwrap()
                .pid
        }

        /// Waits until the fake engine reported readiness on its standard output.
        async fn wait_ready(&self, id: &str) {
            let log = self.directory(id).join("stdout.log");
            wait_until(|| fs::read_to_string(&log).is_ok_and(|text| text.contains(" ready"))).await;
        }

        /// Identity the running workload reports: its loopback bindings plus the engine process
        /// named by the launcher's process record.
        fn serving_identity(
            &self,
            launch: &WorkloadLaunch,
            ports: &BTreeMap<String, u16>,
        ) -> ObservedWorkload {
            let id = workload_id(&launch.correlation);
            let record = ProcessRecord::decode(
                &fs::read(self.directory(&id).join(PROCESS_FILE)).expect("process record"),
            )
            .expect("decode process record");
            let mut identity = serving_identity(launch, ports);
            identity.host_process = Some(HostProcessIdentity {
                managed_node_id: self.fixture.config.site_node_id.clone(),
                process_id: record.pid,
                process_start_token: record.start_token,
            });
            identity
        }

        /// Waits until the fake engine logged `text` on either stream.
        async fn wait_logged(&self, id: &str, text: &str) {
            let logs = ["stdout.log", "stderr.log"].map(|name| self.directory(id).join(name));
            wait_until(|| {
                logs.iter()
                    .any(|log| fs::read_to_string(log).is_ok_and(|content| content.contains(text)))
            })
            .await;
        }

        fn recorded_ports(&self, id: &str) -> BTreeMap<String, u16> {
            let spec = LaunchSpec::decode(&fs::read(self.directory(id).join(LAUNCH_FILE)).unwrap())
                .unwrap();
            NativeOwnerRecord::decode(&spec.owner).unwrap().ports
        }
    }

    fn backend_for(fixture: &Fixture) -> NativeRuntimeBackend {
        NativeRuntimeBackend::new(
            NativeRuntimeConfig {
                lifetime: NativeWorkloadLifetime::Ephemeral,
                ..fixture.config.clone()
            },
            launcher_binary(),
        )
        .expect("native backend")
        .with_port_settle_window(Timing::test().native_port_settle_window)
    }

    /// Identity of a running workload with the loopback bindings of its assigned ports.
    fn serving_identity(
        launch: &WorkloadLaunch,
        ports: &BTreeMap<String, u16>,
    ) -> ObservedWorkload {
        let mut identity = expected_identity(launch);
        identity.loopback_endpoints = launch
            .spec
            .ports
            .iter()
            .map(|port| LoopbackEndpoint {
                port_name: port.name.clone(),
                host_port: ports[&port.name],
            })
            .collect();
        identity.services = identity
            .loopback_endpoints
            .iter()
            .map(|endpoint| {
                WorkloadServiceBinding::loopback(
                    identity.workload_id.0.clone(),
                    endpoint.port_name.clone(),
                    u64::from(endpoint.host_port),
                    endpoint.clone(),
                )
            })
            .collect();
        identity
    }

    fn expected_identity(launch: &WorkloadLaunch) -> ObservedWorkload {
        ObservedWorkload {
            workload_id: WorkloadId(workload_id(&launch.correlation)),
            correlation: launch.correlation.clone(),
            backend_id: RuntimeBackendId(BACKEND_ID.to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: None,
        }
    }

    fn log_source() -> RuntimeLogSource {
        RuntimeLogSource {
            name: "engine".to_owned(),
            label: "Engine".to_owned(),
        }
    }

    fn log_query(maximum: u16, before: Option<SystemTime>) -> RuntimeWorkloadLogQuery {
        RuntimeWorkloadLogQuery::new(
            log_source(),
            RuntimeLogQuery::new(std::num::NonZeroU16::new(maximum).unwrap(), before).unwrap(),
        )
    }

    fn follow_query(
        since: Option<SystemTime>,
        cancellation: CancellationToken,
    ) -> RuntimeWorkloadLogFollowQuery {
        RuntimeWorkloadLogFollowQuery::new(
            log_source(),
            RuntimeLogFollowQuery::new(since, cancellation),
        )
    }

    /// Text and stream of each entry, oldest first.
    fn lines(history: &RuntimeLogHistory) -> Vec<(&str, RuntimeLogStreamKind)> {
        history
            .entries
            .iter()
            .map(|entry| (std::str::from_utf8(&entry.content).unwrap(), entry.stream))
            .collect()
    }

    async fn next_event(stream: &mut RuntimeLogStream) -> RuntimeLogEvent {
        tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("event in time")
            .expect("stream open")
            .expect("log event")
    }

    async fn drain_to_end(stream: &mut RuntimeLogStream) -> Vec<RuntimeLogEvent> {
        let mut events = Vec::new();
        while let Some(event) = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("stream closes")
        {
            events.push(event.expect("log event"));
        }
        events
    }

    /// Replaces a read-only record file, as a person editing it by hand would.
    fn replace_file(path: &Path, contents: &[u8]) {
        fs::remove_file(path).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
}
