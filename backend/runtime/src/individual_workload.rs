//! Owner-neutral contract for individual (single-container) workload backends: the
//! workload lifecycle — discover, create, start, inspect, stop, remove — plus bounded
//! log read and follow operations, and the launch, observation, identity, and log types
//! the contract exchanges.
//!
//! A backend never owns catalog data, desired site configuration, or current-state
//! persistence; it runs workloads and reports observations, and each configured backend
//! serves one exact execution target.

use std::{num::NonZeroU16, pin::Pin, sync::Arc, time::SystemTime};

use dotcore_domain::ExecutionPlatform;
use dotcore_domain::SiteNodeId;
use dotcore_domain::real_time;
use futures_util::Stream;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{RuntimeBackendError, RuntimeFuture, WorkloadSpec};

pub const MAX_RUNTIME_LOG_ENTRIES: u16 = 200;
pub const MAX_RUNTIME_LOG_SOURCES: usize = 32;

/// Boxed stream of direct runtime log events.
pub type RuntimeLogStream =
    Pin<Box<dyn Stream<Item = Result<RuntimeLogEvent, RuntimeBackendError>> + Send>>;

/// Boxed future returned by one direct runtime log operation.
pub type RuntimeLogFuture<'a, T> =
    Pin<Box<dyn std::future::Future<Output = Result<T, RuntimeBackendError>> + Send + 'a>>;

/// Controller-facing adapter for workload lifecycle and observations.
///
/// A runtime backend does not own catalog data, desired site configuration, or
/// current-state persistence.
pub trait RuntimeBackend: Send + Sync {
    /// Identifies the backend instance that owns its private workload identities.
    fn backend_id(&self) -> RuntimeBackendId;

    /// Reports the effective destination environment rather than the controller process platform.
    fn execution_platform<'a>(&'a self) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::ExecutionPlatformUnavailable,
        )))
    }

    /// Declares which launch assets the controller must physically materialize.
    fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
        WorkloadMaterializationRequirements::physical()
    }

    /// Reports whether the backend can realize explicit host-shared IPC intent.
    fn supports_host_shared_ipc(&self) -> bool {
        false
    }

    /// Reports runtime-owned activity before workload creation for one admitted realization.
    fn prepare_realization<'a>(
        &'a self,
        _: &'a WorkloadStartupRequest,
        _: CancellationToken,
        _: Option<WorkloadStartupObserver>,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }

    /// Measures the memory one workload's processes use right now, for resource attribution.
    ///
    /// The cost grows with the number of processes the workload runs, so callers sample it on
    /// their own interval instead of with every inspection. Runtimes that cannot attribute
    /// process memory report it as not reported.
    fn observe_process_memory<'a>(
        &'a self,
        _: &'a WorkloadId,
    ) -> RuntimeFuture<'a, ProcessMemoryObservation> {
        Box::pin(std::future::ready(Ok(ProcessMemoryObservation {
            observed_at: real_time::wall_now(),
            memory: ProcessMemory::NotReported,
            process: None,
        })))
    }

    /// Discovers all workloads that this backend can safely identify as Dotcore-owned.
    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>>;

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation>;

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation>;

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation>;

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()>;

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()>;

    /// Reads direct diagnostics only when this runtime explicitly supports the capability.
    fn read_workload_logs<'a>(
        &'a self,
        _: &'a WorkloadId,
        _: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }

    /// Follows direct diagnostics only when this runtime explicitly supports the capability.
    fn follow_workload_logs<'a>(
        &'a self,
        _: &'a WorkloadId,
        _: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }

    /// Reads diagnostics from the exactly retained workload with this full owner correlation.
    fn read_workload_logs_by_correlation<'a>(
        &'a self,
        _: &'a WorkloadCorrelation,
        _: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }

    /// Follows diagnostics from the exactly retained workload with this full owner correlation.
    fn follow_workload_logs_by_correlation<'a>(
        &'a self,
        _: &'a WorkloadCorrelation,
        _: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }
}

/// Memory one workload's processes used at one instant.
#[derive(Debug)]
pub struct ProcessMemoryObservation {
    pub observed_at: SystemTime,
    pub memory: ProcessMemory,
    /// The main process measured, when the runtime identifies it, so a caller can tell a
    /// relaunched process from the one it expects.
    pub process: Option<dotcore_domain::host_process::HostProcessIdentity>,
}

/// Memory of a workload's processes, or why it has no figure.
#[derive(Debug)]
pub enum ProcessMemory {
    /// Bytes used by the workload's main process and every process it started, read in full.
    Available { bytes: u64 },

    /// The runtime attributes process memory but could not read a complete figure; a partial
    /// sum is never reported.
    Unavailable(ProcessMemoryUnavailable),

    /// The runtime does not attribute memory to workload processes.
    NotReported,
}

/// Why a runtime that attributes process memory has no figure for a workload.
#[derive(Debug, thiserror::Error)]
pub enum ProcessMemoryUnavailable {
    #[error("workload process is not running")]
    NotRunning,

    #[error("workload process memory could not be read")]
    Read(#[source] anyhow::Error),
}

/// Physical launch assets that a runtime implementation requires before realization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkloadMaterializationRequirements {
    /// Whether the executable artifact, an OCI image or a Nix store path, must be prepared.
    pub executable_artifact: WorkloadAssetMaterialization,
    pub host_mounts: WorkloadAssetMaterialization,
}

impl WorkloadMaterializationRequirements {
    pub const fn physical() -> Self {
        Self {
            executable_artifact: WorkloadAssetMaterialization::Physical,
            host_mounts: WorkloadAssetMaterialization::Physical,
        }
    }

    pub const fn virtualized() -> Self {
        Self {
            executable_artifact: WorkloadAssetMaterialization::ReferenceOnly,
            host_mounts: WorkloadAssetMaterialization::ReferenceOnly,
        }
    }
}

/// Whether one class of launch asset must exist physically before realization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadAssetMaterialization {
    Physical,

    ReferenceOnly,
}

/// Controller-issued context for one runtime realization preparation attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadStartupRequest {
    pub owner_id: String,
    pub provenance: WorkloadRealizationProvenance,
}

/// Receives cumulative runtime startup activity without assigning product status.
pub type WorkloadStartupObserver = Arc<dyn Fn(WorkloadStartupActivity) + Send + Sync>;
/// Controller-authoritative reason for admitting one realization attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadRealizationProvenance {
    FirstActivation,

    Recovery,

    Resume,

    Replacement,
}

/// Runtime-owned startup activity attached to an exact workload observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadStartupActivity {
    pub phase: WorkloadStartupPhase,
    pub progress: Option<WorkloadStartupProgress>,
    pub detail: Option<String>,
}

/// Portable stage of workload realization without product status semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadStartupPhase {
    AcquiringAssets,

    StartingWorkload,
}

/// Cumulative runtime startup progress in a declared unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WorkloadStartupProgress {
    pub completed: u64,
    pub total: Option<u64>,
    pub unit: WorkloadStartupProgressUnit,
}

/// Unit carried by cumulative runtime startup progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadStartupProgressUnit {
    Bytes,
}

/// Opaque reference to one named service exposed by an exact realization.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkloadServiceReference {
    pub realization_id: String,
    pub service_name: String,
    pub revision: u64,
}

/// One opaque named service binding reported by runtime for an exact realization.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadServiceBinding {
    pub reference: WorkloadServiceReference,
    pub access: WorkloadServiceAccess,
}

impl WorkloadServiceBinding {
    pub fn loopback(
        realization_id: impl Into<String>,
        service_name: impl Into<String>,
        revision: u64,
        endpoint: LoopbackEndpoint,
    ) -> Self {
        let service_name = service_name.into();
        Self {
            reference: WorkloadServiceReference {
                realization_id: realization_id.into(),
                service_name,
                revision,
            },
            access: WorkloadServiceAccess::Loopback(endpoint),
        }
    }

    pub fn native(
        realization_id: impl Into<String>,
        service_name: impl Into<String>,
        revision: u64,
    ) -> Self {
        Self {
            reference: WorkloadServiceReference {
                realization_id: realization_id.into(),
                service_name: service_name.into(),
                revision,
            },
            access: WorkloadServiceAccess::Native,
        }
    }
}

/// How a caller reaches one runtime-reported service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadServiceAccess {
    /// A host loopback endpoint published by the workload.
    Loopback(LoopbackEndpoint),

    /// An in-process native provider, reached without a network endpoint.
    Native,
}

/// Identifies one runtime-private workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadId(pub String);

/// Identifies one runtime backend without exposing its native identities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeBackendId(pub String);

/// Bounded point-in-time request for direct runtime diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeLogQuery {
    maximum_entries: NonZeroU16,
    before_timestamp: Option<SystemTime>,
}

impl RuntimeLogQuery {
    /// Creates a bounded query for the newest entries before an optional timestamp.
    pub fn new(
        maximum_entries: NonZeroU16,
        before_timestamp: Option<SystemTime>,
    ) -> Result<Self, RuntimeLogQueryError> {
        if maximum_entries.get() > MAX_RUNTIME_LOG_ENTRIES {
            return Err(RuntimeLogQueryError::MaximumEntries);
        }
        Ok(Self {
            maximum_entries,
            before_timestamp,
        })
    }

    pub fn maximum_entries(&self) -> NonZeroU16 {
        self.maximum_entries
    }

    pub fn before_timestamp(&self) -> Option<SystemTime> {
        self.before_timestamp
    }
}

/// Bounded direct runtime diagnostics query for one controller-issued public source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeWorkloadLogQuery {
    source: RuntimeLogSource,
    history: RuntimeLogQuery,
}

impl RuntimeWorkloadLogQuery {
    /// Associates one public source descriptor with a bounded history query.
    pub fn new(source: RuntimeLogSource, history: RuntimeLogQuery) -> Self {
        Self { source, history }
    }

    pub fn source(&self) -> &RuntimeLogSource {
        &self.source
    }

    pub fn history(&self) -> &RuntimeLogQuery {
        &self.history
    }
}

/// Bounded direct runtime diagnostics follow request.
#[derive(Clone, Debug)]
pub struct RuntimeLogFollowQuery {
    since_timestamp: Option<SystemTime>,
    cancellation: CancellationToken,
}

impl RuntimeLogFollowQuery {
    /// Creates a follow request that stops when its cancellation token is cancelled.
    pub fn new(since_timestamp: Option<SystemTime>, cancellation: CancellationToken) -> Self {
        Self {
            since_timestamp,
            cancellation,
        }
    }

    pub fn since_timestamp(&self) -> Option<SystemTime> {
        self.since_timestamp
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Direct runtime follow request for one controller-issued public source.
#[derive(Clone, Debug)]
pub struct RuntimeWorkloadLogFollowQuery {
    source: RuntimeLogSource,
    follow: RuntimeLogFollowQuery,
}

impl RuntimeWorkloadLogFollowQuery {
    /// Associates one public source descriptor with a follow query.
    pub fn new(source: RuntimeLogSource, follow: RuntimeLogFollowQuery) -> Self {
        Self { source, follow }
    }

    pub fn source(&self) -> &RuntimeLogSource {
        &self.source
    }

    pub fn follow(&self) -> &RuntimeLogFollowQuery {
        &self.follow
    }
}

/// A stable owner-issued public identity for one runtime log source.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RuntimeLogSource {
    /// Stable public identity used for filtering and follow resumption.
    pub name: String,
    /// Controller-issued public label displayed with every record.
    pub label: String,
}

/// Bounded selection of stable public runtime log sources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeLogSourceSelection(Vec<RuntimeLogSource>);

impl RuntimeLogSourceSelection {
    /// Creates a selection that preserves every requested public source identity.
    pub fn new(sources: Vec<RuntimeLogSource>) -> Result<Self, RuntimeLogQueryError> {
        if sources.len() > MAX_RUNTIME_LOG_SOURCES {
            return Err(RuntimeLogQueryError::MaximumSources);
        }
        let unique_names = sources
            .iter()
            .map(|source| source.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if unique_names.len() != sources.len() {
            return Err(RuntimeLogQueryError::DuplicateSource);
        }
        Ok(Self(sources))
    }

    pub fn sources(&self) -> &[RuntimeLogSource] {
        &self.0
    }
}

/// One direct runtime log record without runtime-native identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeLogEntry {
    pub source: RuntimeLogSource,
    pub timestamp: SystemTime,
    pub stream: RuntimeLogStreamKind,
    pub content: Vec<u8>,
    pub truncated: bool,
}

/// The workload stream that produced a direct runtime log record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeLogStreamKind {
    Stdout,

    Stderr,
}

/// Point-in-time direct runtime diagnostic history and bounded-acquisition evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeLogHistory {
    pub entries: Vec<RuntimeLogEntry>,
    /// Whether runtime observed output older than the returned entries.
    pub has_older_output: bool,
    /// Whether runtime discarded candidate entries to stay within its acquisition byte budget.
    pub candidate_budget_truncated: bool,
}

/// A direct runtime follow event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeLogEvent {
    Entry(RuntimeLogEntry),

    SourceUnavailable(RuntimeLogSource),
}

/// Invalid bounded direct runtime diagnostics input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RuntimeLogQueryError {
    #[error("Runtime log entry limit exceeds the supported maximum")]
    MaximumEntries,

    #[error("Runtime log source selection exceeds the supported maximum")]
    MaximumSources,

    #[error("Runtime log source selection contains duplicate names")]
    DuplicateSource,

    #[error("Runtime log resume position names an unselected source")]
    UnselectedResumeSource,
}

/// Canonical execution target resolved by an owner before runtime invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionTarget {
    pub managed_node_id: SiteNodeId,
    pub accelerator_device_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadLaunch {
    pub correlation: WorkloadCorrelation,
    pub execution_target: ExecutionTarget,
    pub spec: WorkloadSpec,
}

impl WorkloadLaunch {
    /// Builds a workload launch with its durable owner correlation.
    pub fn new(
        owner_id: impl Into<String>,
        execution_target: ExecutionTarget,
        spec: WorkloadSpec,
    ) -> Self {
        let reconcile_generation = spec.reconcile_generation(&execution_target);
        Self {
            correlation: WorkloadCorrelation {
                owner_id: owner_id.into(),
                reconcile_generation,
            },
            execution_target,
            spec,
        }
    }

    /// Rejects explicit realization requirements unsupported by the selected backend.
    pub fn validate_backend_capabilities(
        &self,
        backend: &dyn RuntimeBackend,
    ) -> Result<(), RuntimeBackendError> {
        if self.spec.ipc_mode == WorkloadIpcMode::HostShared && !backend.supports_host_shared_ipc()
        {
            return Err(RuntimeBackendError::host_shared_ipc_unavailable());
        }
        Ok(())
    }

    /// Stable digest of the exact launch intent, recorded on the workload so a later observation
    /// can tell whether it still matches the launch.
    pub fn exact_intent_fingerprint(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"dotcore-workload-exact-intent-fingerprint-v1\0");
        update_fingerprint(&mut digest, self.correlation.owner_id.as_bytes());
        update_fingerprint(
            &mut digest,
            &self.correlation.reconcile_generation.to_le_bytes(),
        );
        update_fingerprint(
            &mut digest,
            self.execution_target.managed_node_id.as_str().as_bytes(),
        );
        for device in &self.execution_target.accelerator_device_ids {
            update_fingerprint(&mut digest, device.as_bytes());
        }
        self.spec.update_exact_intent(&mut digest);
        format!("{:x}", digest.finalize())
    }
}

pub(super) fn update_fingerprint(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_le_bytes());
    digest.update(value);
}

/// Opaque owner correlation that runtime preserves without interpreting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadCorrelation {
    pub owner_id: String,
    pub reconcile_generation: u64,
}

/// IPC namespace isolation requested by a workload owner.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorkloadIpcMode {
    /// Uses a private IPC namespace that is not shared with the host.
    #[default]
    Isolated,

    /// Uses the host IPC namespace.
    HostShared,
}

impl WorkloadIpcMode {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Isolated => "isolated",
            Self::HostShared => "host-shared",
        }
    }
}

/// One named, node-local storage folder the runtime provisions and mounts into a workload.
///
/// The runtime chooses the host folder from the kind, namespace, key and name, so every workload
/// naming the same four values shares one folder and it survives container recreation. The owner
/// picks the namespace and key to set who shares it, for example one deployment or every
/// deployment of one application; the runtime treats both as opaque names. The owner also says
/// whether the storage expires once no workload has used it for a while, which machine cleanup
/// relies on for storage that outlives any single workload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadStorageAttachment {
    pub name: String,
    pub destination: String,
    pub kind: WorkloadStorageKind,
    pub namespace: String,
    pub key: String,
    pub read_only: bool,
    pub expires_when_unused: bool,
}

impl WorkloadStorageAttachment {
    /// Returns the owner key whose storage holds this attachment's folder.
    pub fn storage_key(&self) -> WorkloadStorageKey {
        WorkloadStorageKey {
            kind: self.kind,
            namespace: self.namespace.clone(),
            key: self.key.clone(),
        }
    }
}

/// The storage one owner holds under one kind, namespace and key, across every attachment name.
///
/// Owners use it to check or remove the storage they resolved for one scope, such as one
/// deployment's data, without the runtime knowing what the namespace or key mean.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkloadStorageKey {
    pub kind: WorkloadStorageKind,
    pub namespace: String,
    pub key: String,
}

/// What a storage attachment holds, which decides where the runtime keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum WorkloadStorageKind {
    /// Recreatable scratch, such as compile caches.
    Cache,

    /// Content the workload's users create and keep.
    Data,
}

impl WorkloadStorageKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cache => "cache",
            Self::Data => "data",
        }
    }
}

/// A runtime-assigned loopback endpoint for a declared workload port.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopbackEndpoint {
    pub port_name: String,
    pub host_port: u16,
}

/// Backend-private workload identity included with every conclusive observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedWorkload {
    pub workload_id: WorkloadId,
    pub correlation: WorkloadCorrelation,
    pub backend_id: RuntimeBackendId,
    pub loopback_endpoints: Vec<LoopbackEndpoint>,
    pub services: Vec<WorkloadServiceBinding>,
    pub host_process: Option<dotcore_domain::host_process::HostProcessIdentity>,
}

/// An infrastructure-neutral observation that cannot combine contradictory state and outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadObservation {
    Starting(ObservedWorkload),

    Running(ObservedWorkload),

    Stopping(ObservedWorkload),

    /// A known workload exists but is stopped and can be restarted or replaced.
    Stopped {
        workload: ObservedWorkload,
        exit_status: Option<i64>,
    },

    /// A known workload is observed but cannot currently serve.
    Unavailable(ObservedWorkload),

    /// The backend conclusively confirmed that the expected workload no longer exists.
    Absent(ObservedWorkload),

    /// Discovery did not produce a safe Dotcore workload identity or lifecycle conclusion.
    Unknown {
        workload_id: WorkloadId,
        backend_id: RuntimeBackendId,
    },
}

impl WorkloadObservation {
    /// Reports a stopped workload when runtime has no terminal process status.
    pub fn stopped(workload: ObservedWorkload) -> Self {
        Self::Stopped {
            workload,
            exit_status: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use super::super::{
        WorkloadArtifactBinding, WorkloadExecutable, WorkloadExecutableArtifact, WorkloadPort,
        WorkloadValue,
    };
    use super::*;

    struct ThirdRuntime;

    impl RuntimeBackend for ThirdRuntime {
        fn backend_id(&self) -> RuntimeBackendId {
            RuntimeBackendId("runtime/contract-third".to_owned())
        }

        fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
            WorkloadMaterializationRequirements::virtualized()
        }

        fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
            Box::pin(async { unreachable!("contract test does not discover workloads") })
        }

        fn create_workload<'a>(
            &'a self,
            _: &'a WorkloadLaunch,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(async { unreachable!("contract test does not create workloads") })
        }

        fn start_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(async { unreachable!("contract test does not start workloads") })
        }

        fn inspect_workload<'a>(
            &'a self,
            _: &'a WorkloadId,
        ) -> RuntimeFuture<'a, WorkloadObservation> {
            Box::pin(async { unreachable!("contract test does not inspect workloads") })
        }

        fn stop_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(async { unreachable!("contract test does not stop workloads") })
        }

        fn remove_workload<'a>(&'a self, _: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
            Box::pin(async { unreachable!("contract test does not remove workloads") })
        }
    }

    #[test]
    fn realization_contracts_distinguish_physical_and_virtual_assets() {
        let runtime = ThirdRuntime;

        assert_eq!(runtime.backend_id().0, "runtime/contract-third");
        assert_eq!(
            runtime.materialization_requirements(),
            WorkloadMaterializationRequirements::virtualized()
        );
        assert!(!runtime.supports_host_shared_ipc());
        assert_eq!(
            WorkloadMaterializationRequirements::physical(),
            WorkloadMaterializationRequirements {
                executable_artifact: WorkloadAssetMaterialization::Physical,
                host_mounts: WorkloadAssetMaterialization::Physical,
            }
        );
        assert_eq!(
            WorkloadMaterializationRequirements::virtualized(),
            WorkloadMaterializationRequirements {
                executable_artifact: WorkloadAssetMaterialization::ReferenceOnly,
                host_mounts: WorkloadAssetMaterialization::ReferenceOnly,
            }
        );
        assert_eq!(
            WorkloadRealizationProvenance::FirstActivation,
            WorkloadRealizationProvenance::FirstActivation
        );
        assert_eq!(
            WorkloadStartupProgress {
                completed: 2,
                total: Some(8),
                unit: WorkloadStartupProgressUnit::Bytes,
            },
            WorkloadStartupProgress {
                completed: 2,
                total: Some(8),
                unit: WorkloadStartupProgressUnit::Bytes,
            }
        );
    }

    #[tokio::test]
    async fn runtimes_without_process_attribution_report_no_memory() {
        let observation = ThirdRuntime
            .observe_process_memory(&WorkloadId("workload".to_owned()))
            .await
            .expect("default observation");

        assert!(matches!(observation.memory, ProcessMemory::NotReported));
    }

    #[test]
    fn direct_log_queries_bound_entries_and_sources() {
        assert_eq!(
            RuntimeLogQuery::new(
                NonZeroU16::new(MAX_RUNTIME_LOG_ENTRIES + 1).expect("non-zero entry limit"),
                None,
            ),
            Err(RuntimeLogQueryError::MaximumEntries)
        );
        assert_eq!(
            RuntimeLogSourceSelection::new(
                (0..=MAX_RUNTIME_LOG_SOURCES)
                    .map(|index| RuntimeLogSource {
                        name: format!("unit-{index}"),
                        label: format!("Unit {index}"),
                    })
                    .collect(),
            ),
            Err(RuntimeLogQueryError::MaximumSources)
        );
        assert_eq!(
            RuntimeLogSourceSelection::new(vec![
                RuntimeLogSource {
                    name: "web".to_owned(),
                    label: "Web".to_owned(),
                },
                RuntimeLogSource {
                    name: "web".to_owned(),
                    label: "Renamed".to_owned(),
                },
            ]),
            Err(RuntimeLogQueryError::DuplicateSource)
        );
    }

    #[test]
    fn workload_log_queries_preserve_controller_issued_public_descriptors() {
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };
        let history = RuntimeLogQuery::new(NonZeroU16::new(1).expect("non-zero"), None)
            .expect("bounded history query");
        let query = RuntimeWorkloadLogQuery::new(source.clone(), history);
        let follow = RuntimeWorkloadLogFollowQuery::new(
            source.clone(),
            RuntimeLogFollowQuery::new(None, CancellationToken::new()),
        );

        assert_eq!(query.source(), &source);
        assert_eq!(follow.source(), &source);
        assert_eq!(query.source().name, "model-primary");
        assert_eq!(follow.source().label, "Primary model");
    }

    #[test]
    fn log_source_preserves_controller_issued_public_name_and_label() {
        let source = RuntimeLogSource {
            name: "model-primary".to_owned(),
            label: "Primary model".to_owned(),
        };

        assert_eq!(source.name, "model-primary");
        assert_eq!(source.label, "Primary model");
    }

    #[test]
    fn workload_launch_generation_excludes_host_mount_paths() {
        let target = ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-a"),
            accelerator_device_ids: vec!["gpu-0".to_owned(), "gpu-1".to_owned()],
        };
        let mut spec = WorkloadSpec {
            executable: executable("serve"),
            required_platform: ExecutionPlatform::linux_amd64(),
            arguments: vec![
                WorkloadValue::literal("--port"),
                WorkloadValue::literal("8080"),
            ],
            environment: BTreeMap::from([
                ("A".to_owned(), WorkloadValue::literal("1")),
                ("B".to_owned(), WorkloadValue::literal("2")),
            ]),
            artifacts: vec![
                WorkloadArtifactBinding {
                    name: "cache".to_owned(),
                    source: PathBuf::from("/host/one"),
                    destination: "/cache".to_owned(),
                    read_only: true,
                },
                WorkloadArtifactBinding {
                    name: "data".to_owned(),
                    source: PathBuf::from("/host/two"),
                    destination: "/data".to_owned(),
                    read_only: false,
                },
            ],
            storage: Vec::new(),
            ports: vec![
                WorkloadPort {
                    name: "http".to_owned(),
                    container_port: 8080,
                },
                WorkloadPort {
                    name: "metrics".to_owned(),
                    container_port: 9090,
                },
            ],
            accelerator_device_ids: vec!["gpu-0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        };

        let launch = WorkloadLaunch::new("owner", target.clone(), spec.clone());
        assert_eq!(
            launch.correlation.reconcile_generation,
            11_334_744_919_646_123_664
        );
        assert_eq!(
            launch.exact_intent_fingerprint(),
            "aa9babcdee538169a4794bb7c619705febbccbf80f0700be6fdfcd00f912d8a5"
        );
        assert_ne!(launch.correlation.reconcile_generation, 0);
        let mut host_ipc = spec.clone();
        host_ipc.ipc_mode = WorkloadIpcMode::HostShared;
        let host_ipc_launch = WorkloadLaunch::new("owner", target.clone(), host_ipc);
        assert_ne!(
            launch.correlation.reconcile_generation,
            host_ipc_launch.correlation.reconcile_generation
        );
        assert_ne!(
            launch.exact_intent_fingerprint(),
            host_ipc_launch.exact_intent_fingerprint()
        );
        let mut same_correlation_host_ipc = launch.clone();
        same_correlation_host_ipc.spec.ipc_mode = WorkloadIpcMode::HostShared;
        assert_eq!(launch.correlation, same_correlation_host_ipc.correlation);
        assert_ne!(
            launch.exact_intent_fingerprint(),
            same_correlation_host_ipc.exact_intent_fingerprint()
        );

        assert_eq!(
            launch.correlation.reconcile_generation,
            WorkloadLaunch::new("other-owner", target.clone(), spec.clone())
                .correlation
                .reconcile_generation
        );

        spec.artifacts[0].source = PathBuf::from("/host/other");
        assert_eq!(
            launch.correlation.reconcile_generation,
            WorkloadLaunch::new("owner", target.clone(), spec.clone())
                .correlation
                .reconcile_generation
        );

        spec.artifacts[0].destination = "/other".to_owned();
        assert_ne!(
            launch.correlation.reconcile_generation,
            WorkloadLaunch::new("owner", target, spec)
                .correlation
                .reconcile_generation
        );
    }

    #[test]
    fn storage_attachments_change_generation_and_fingerprint_by_every_identity_part() {
        let target = ExecutionTarget {
            managed_node_id: SiteNodeId::for_test("node-a"),
            accelerator_device_ids: vec!["gpu-0".to_owned()],
        };
        let spec = WorkloadSpec {
            executable: executable("serve"),
            required_platform: ExecutionPlatform::linux_amd64(),
            arguments: Vec::new(),
            environment: BTreeMap::new(),
            artifacts: Vec::new(),
            storage: Vec::new(),
            ports: Vec::new(),
            accelerator_device_ids: vec!["gpu-0".to_owned()],
            required_accelerator_device_count: 1,
            ipc_mode: WorkloadIpcMode::Isolated,
            expected_memory: Default::default(),
        };
        let without = WorkloadLaunch::new("owner", target.clone(), spec.clone());
        let mut stored = spec;
        stored.storage.push(WorkloadStorageAttachment {
            name: "engine-cache".to_owned(),
            destination: "/var/cache/engine".to_owned(),
            kind: WorkloadStorageKind::Cache,
            namespace: "deployment".to_owned(),
            key: "deployment-one".to_owned(),
            read_only: false,
            expires_when_unused: false,
        });
        let with = WorkloadLaunch::new("owner", target.clone(), stored.clone());

        // Workloads without storage keep the identity they had before storage existed.
        assert_eq!(
            without.correlation.reconcile_generation,
            5_857_328_248_594_806_208
        );
        assert_eq!(
            without.exact_intent_fingerprint(),
            "4114f97592bc766e0eedc1bfd746441998fb9611cc4bbf50118995df2d333dcb"
        );
        assert_ne!(
            without.correlation.reconcile_generation,
            with.correlation.reconcile_generation
        );
        assert_ne!(
            without.exact_intent_fingerprint(),
            with.exact_intent_fingerprint()
        );

        let changes: [fn(&mut WorkloadStorageAttachment); 6] = [
            |storage| storage.key = "deployment-two".to_owned(),
            |storage| storage.namespace = "engine-artifact".to_owned(),
            |storage| storage.kind = WorkloadStorageKind::Data,
            |storage| storage.name = "other-cache".to_owned(),
            |storage| storage.destination = "/var/cache/other".to_owned(),
            |storage| storage.read_only = true,
        ];
        for change in changes {
            let mut changed = stored.clone();
            change(&mut changed.storage[0]);
            let changed = WorkloadLaunch::new("owner", target.clone(), changed);
            assert_ne!(
                with.correlation.reconcile_generation, changed.correlation.reconcile_generation,
                "{:?}",
                changed.spec.storage
            );
            assert_ne!(
                with.exact_intent_fingerprint(),
                changed.exact_intent_fingerprint(),
                "{:?}",
                changed.spec.storage
            );
        }
    }

    fn executable(path: &str) -> WorkloadExecutable {
        WorkloadExecutable {
            artifact: WorkloadExecutableArtifact::OciImage("image@sha256:abc".to_owned()),
            path: path.to_owned(),
        }
    }
}
