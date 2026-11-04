//! Workload-group runtime contract, launch graph, and observations.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::Ipv4Addr,
    num::NonZeroU16,
    time::{Duration, SystemTime},
};

use tokio_util::sync::CancellationToken;

use super::{
    ExecutionTarget, RuntimeBackendError, RuntimeBackendId, RuntimeFuture, RuntimeLogFuture,
    RuntimeLogHistory, RuntimeLogQuery, RuntimeLogQueryError, RuntimeLogSource,
    RuntimeLogSourceSelection, RuntimeLogStream, WorkloadCorrelation,
    WorkloadMaterializationRequirements, WorkloadPort, WorkloadStorageAttachment,
    WorkloadStorageKey,
};

pub const MAX_READY_LAUNCH_GATE_PATH_BYTES: usize = 2_048;
pub const MAX_READY_LAUNCH_GATE_TIMEOUT: Duration = Duration::from_secs(300);
pub const MAX_WORKLOAD_GROUP_STALE_REASON_BYTES: usize = 128;

/// Controller-facing adapter for portable workload-group lifecycle and observations.
pub trait WorkloadGroupBackend: Send + Sync {
    /// Identifies the backend instance that owns its private infrastructure.
    fn backend_id(&self) -> RuntimeBackendId;

    /// Reports the effective platform of the addressed execution target.
    fn execution_platform<'a>(
        &'a self,
        execution_target_id: &'a str,
    ) -> RuntimeFuture<'a, dotcore_domain::ExecutionPlatform>;

    /// Declares which launch assets the controller must physically materialize.
    fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
        WorkloadMaterializationRequirements::physical()
    }

    /// Discovers groups that this backend can safely identify as Dotcore-owned.
    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>>;

    /// Reports whether storage already exists under one owner key.
    fn storage_exists<'a>(&'a self, _: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
        Box::pin(std::future::ready(Ok(false)))
    }

    /// Removes every storage folder under one owner key and confirms it is absent. Backends that
    /// retain no storage have nothing to remove.
    fn remove_storage<'a>(&'a self, _: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Ok(())))
    }

    /// Removes and confirms absence of infrastructure with the exact stable group and owner identity.
    fn ensure_workload_group_absent<'a>(
        &'a self,
        _: &'a WorkloadGroupId,
        _: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::invalid_launch(
                "stable-identity workload-group cleanup is unavailable from this runtime backend",
            ),
        )))
    }

    /// Creates the requested group without changing controller-owned state.
    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation>;

    /// Starts retained, exactly owned units without creating missing infrastructure.
    fn start_workload_group<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::invalid_launch(
                "workload-group start is unavailable from this runtime backend",
            ),
        )))
    }

    /// Inspects the requested group without mutating its infrastructure.
    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation>;

    /// Removes all exactly owned infrastructure for the requested group.
    fn remove_workload_group<'a>(&'a self, group: &'a WorkloadGroupLaunch)
    -> RuntimeFuture<'a, ()>;

    /// Removes all infrastructure described by an exact previously observed group.
    ///
    /// This capability is used when the controller has a newer launch projection but must first
    /// remove the older, still-owned group configuration.
    fn remove_observed_workload_group<'a>(
        &'a self,
        _group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::invalid_launch(
                "observed workload-group removal is unavailable from this runtime backend",
            ),
        )))
    }

    /// Stops retained, exactly owned active units without removing infrastructure.
    fn stop_workload_group<'a>(&'a self, _: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::invalid_launch(
                "workload-group stop is unavailable from this runtime backend",
            ),
        )))
    }

    /// Reads bounded retained diagnostics from exact named units in this workload group.
    fn read_workload_group_logs<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
        _: &'a RuntimeLogSourceSelection,
        _: &'a RuntimeLogQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }

    /// Opens one independently resumable diagnostics stream for each exact named unit.
    fn follow_workload_group_logs<'a>(
        &'a self,
        _: &'a WorkloadGroupLaunch,
        _: &'a RuntimeLogSourceSelection,
        _: RuntimeGroupLogFollowQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
        Box::pin(std::future::ready(Err(
            RuntimeBackendError::log_capability_unavailable(),
        )))
    }
}

/// One source-specific history acquisition outcome.
#[derive(Debug)]
pub struct RuntimeLogSourceHistory {
    pub source: RuntimeLogSource,
    pub result: Result<RuntimeLogHistory, RuntimeBackendError>,
}

/// One source-specific follow acquisition outcome.
pub struct RuntimeLogSourceStream {
    pub source: RuntimeLogSource,
    pub result: Result<RuntimeLogStream, RuntimeBackendError>,
}

/// Independently resumable follow request for a workload group.
#[derive(Clone, Debug)]
pub struct RuntimeGroupLogFollowQuery {
    resume_timestamps: BTreeMap<String, SystemTime>,
    cancellation: CancellationToken,
}

impl RuntimeGroupLogFollowQuery {
    /// Creates a group follow request whose positions all belong to the selected sources.
    pub fn new(
        selection: &RuntimeLogSourceSelection,
        resume_timestamps: BTreeMap<String, SystemTime>,
        cancellation: CancellationToken,
    ) -> Result<Self, RuntimeLogQueryError> {
        let selected = selection
            .sources()
            .iter()
            .map(|source| source.name.as_str())
            .collect::<BTreeSet<_>>();
        if resume_timestamps
            .keys()
            .any(|name| !selected.contains(name.as_str()))
        {
            return Err(RuntimeLogQueryError::UnselectedResumeSource);
        }
        Ok(Self {
            resume_timestamps,
            cancellation,
        })
    }

    pub fn resume_timestamp(&self, source_name: &str) -> Option<SystemTime> {
        self.resume_timestamps.get(source_name).copied()
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

/// Portable controller-assigned identity for one workload group.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkloadGroupId(pub String);

/// Stable name for one unit within a workload group.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WorkloadUnitName(pub String);

/// One portable workload group requested by a controller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadGroupLaunch {
    pub group_id: WorkloadGroupId,
    pub correlation: WorkloadCorrelation,
    pub execution_target: ExecutionTarget,
    pub execution_target_id: String,
    pub required_platform: dotcore_domain::ExecutionPlatform,
    pub units: BTreeMap<WorkloadUnitName, WorkloadUnitLaunch>,
    pub publication: WorkloadPublicationRequest,
    /// Site host names the units reach by name. A runtime that gives units their own name
    /// resolution resolves them on the hosting node when it creates the units and pins the results
    /// as host entries, so units never depend on their own DNS for site hosts.
    pub host_aliases: BTreeSet<dotcore_domain::SiteNodeHost>,
}

/// One named runtime-neutral OCI unit in a workload group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadUnitLaunch {
    /// Exact immutable image identity selected and retained by the owning controller.
    pub image: String,
    /// Exec-form command that replaces the image entrypoint. Empty keeps the image entrypoint.
    pub command: Vec<String>,
    /// Exec-form arguments that replace the image's default arguments. Empty keeps them.
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    /// Environment variables whose values may be recomputed while recovering retained infrastructure.
    pub reconfigurable_environment: BTreeSet<String>,
    pub storage: Vec<WorkloadStorageAttachment>,
    pub ports: Vec<WorkloadPort>,
    pub dependencies: Vec<WorkloadUnitDependency>,
    /// A bounded launch gate for READY dependents. It is not authoritative readiness.
    pub ready_launch_gate: Option<ReadyLaunchGate>,
}

/// One condition on another named unit in the same group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadUnitDependency {
    pub unit_name: WorkloadUnitName,
    pub condition: WorkloadDependencyCondition,
}

/// The launch condition that a dependency unit must meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadDependencyCondition {
    Started,

    Ready,
}

/// The one unit port that runtime must publish on the configured host, at the host port its
/// owner chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadPublicationRequest {
    pub unit_name: WorkloadUnitName,
    pub port_name: String,
    pub configured_host: Ipv4Addr,

    /// Requested host-visible port, chosen by the owner and stable for the workload's lifetime.
    pub host_port: NonZeroU16,
}

/// A bounded HTTP check that releases READY dependents during group launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyLaunchGate {
    port_name: String,
    path: String,
    timeout: Duration,
    interval: Duration,
}

impl ReadyLaunchGate {
    /// Builds a bounded, non-authoritative HTTP launch gate.
    pub fn http_get(
        port_name: impl Into<String>,
        path: impl Into<String>,
        timeout: Duration,
        interval: Duration,
    ) -> Result<Self, ReadyLaunchGateError> {
        let port_name = port_name.into();
        let path = path.into();
        if port_name.is_empty() || port_name.len() > 128 || port_name.chars().any(char::is_control)
        {
            return Err(ReadyLaunchGateError::InvalidPortName);
        }
        if path.is_empty()
            || path.len() > MAX_READY_LAUNCH_GATE_PATH_BYTES
            || !path.starts_with('/')
            || path.chars().any(char::is_control)
        {
            return Err(ReadyLaunchGateError::InvalidHttpPath);
        }
        if timeout.is_zero() || timeout > MAX_READY_LAUNCH_GATE_TIMEOUT {
            return Err(ReadyLaunchGateError::InvalidTimeout);
        }
        if interval.is_zero() || interval > timeout {
            return Err(ReadyLaunchGateError::InvalidInterval);
        }
        Ok(Self {
            port_name,
            path,
            timeout,
            interval,
        })
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }
}

/// Invalid bounded launch-gate input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ReadyLaunchGateError {
    #[error("READY launch-gate port name is invalid")]
    InvalidPortName,

    #[error("READY launch-gate HTTP path is invalid")]
    InvalidHttpPath,

    #[error("READY launch-gate timeout is invalid")]
    InvalidTimeout,

    #[error("READY launch-gate interval is invalid")]
    InvalidInterval,
}

/// Valid owned infrastructure observed for one portable workload group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedWorkloadGroup {
    pub group_id: WorkloadGroupId,
    pub correlation: WorkloadCorrelation,
    pub backend_id: RuntimeBackendId,
    pub units: BTreeMap<WorkloadUnitName, WorkloadUnitObservation>,
    pub assigned_endpoint: Option<AssignedWorkloadEndpoint>,
}

/// Infrastructure state and bounded launch-gate result for one named unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkloadUnitObservation {
    pub state: WorkloadUnitState,
    pub ready_launch_gate: Option<ReadyLaunchGateOutcome>,
    pub host_process: Option<dotcore_domain::host_process::HostProcessIdentity>,
}

/// Infrastructure lifecycle state for one named workload unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadUnitState {
    Starting,

    Running,

    Stopping,

    Stopped,

    Unavailable,

    Absent,
}

/// Non-authoritative result of a bounded READY launch gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyLaunchGateOutcome {
    Passed,

    TimedOut,

    Failed(ReadyLaunchGateFailure),
}

/// Safe failure category for a READY launch gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyLaunchGateFailure {
    ConnectionFailed,

    InvalidResponse,
}

/// Runtime-assigned endpoint for the one configured-host publication request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignedWorkloadEndpoint {
    pub unit_name: WorkloadUnitName,
    pub port_name: String,
    pub configured_host: Ipv4Addr,
    pub assigned_port: NonZeroU16,
}

/// A group observation that preserves owned, absent, and inconclusive states.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkloadGroupObservation {
    Observed(ObservedWorkloadGroup),

    /// Valid owned infrastructure differs from the requested configuration.
    StaleConfiguration {
        group: ObservedWorkloadGroup,
        reason: WorkloadGroupStaleReason,
    },

    /// Runtime conclusively confirmed that the expected group does not exist.
    Absent {
        group_id: WorkloadGroupId,
        correlation: WorkloadCorrelation,
        backend_id: RuntimeBackendId,
    },

    /// Discovery did not produce a safe owned-group or lifecycle conclusion.
    Unknown {
        group_id: WorkloadGroupId,
        backend_id: RuntimeBackendId,
    },
}

/// Closed safe reason for valid owned stale group infrastructure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkloadGroupStaleReason {
    PublicationHostChanged,

    PublicationPortChanged,
}

impl WorkloadGroupStaleReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PublicationHostChanged => "configured publication host changed",
            Self::PublicationPortChanged => "requested publication port changed",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        net::Ipv4Addr,
        num::NonZeroU16,
        time::Duration,
    };

    use dotcore_domain::SiteNodeId;

    use super::super::ExecutionTarget;
    use super::*;
    #[test]
    fn group_follow_preserves_independent_selected_resume_positions() {
        let selection = RuntimeLogSourceSelection::new(vec![
            RuntimeLogSource {
                name: "cache".to_owned(),
                label: "Cache".to_owned(),
            },
            RuntimeLogSource {
                name: "web".to_owned(),
                label: "Web service".to_owned(),
            },
        ])
        .expect("unique sources");
        let cache_timestamp = std::time::UNIX_EPOCH + Duration::from_nanos(11);
        let query = RuntimeGroupLogFollowQuery::new(
            &selection,
            BTreeMap::from([("cache".to_owned(), cache_timestamp)]),
            CancellationToken::new(),
        )
        .expect("selected resume position");

        assert_eq!(query.resume_timestamp("cache"), Some(cache_timestamp));
        assert_eq!(query.resume_timestamp("web"), None);
        assert_eq!(
            RuntimeGroupLogFollowQuery::new(
                &selection,
                BTreeMap::from([("worker".to_owned(), cache_timestamp)]),
                CancellationToken::new(),
            )
            .expect_err("unselected resume source"),
            RuntimeLogQueryError::UnselectedResumeSource
        );
    }

    #[test]
    fn workload_group_launch_preserves_portable_graph_and_controller_correlation() {
        let gate = ReadyLaunchGate::http_get(
            "http",
            "/health",
            Duration::from_secs(30),
            Duration::from_secs(1),
        )
        .expect("valid launch gate");
        let units = BTreeMap::from([
            (
                WorkloadUnitName("cache".to_owned()),
                WorkloadUnitLaunch {
                    image: "registry.example/cache@sha256:123".to_owned(),
                    command: vec!["cache".to_owned()],
                    arguments: Vec::new(),
                    environment: BTreeMap::new(),
                    reconfigurable_environment: BTreeSet::new(),
                    storage: Vec::new(),
                    ports: Vec::new(),
                    dependencies: Vec::new(),
                    ready_launch_gate: None,
                },
            ),
            (
                WorkloadUnitName("database".to_owned()),
                WorkloadUnitLaunch {
                    image: "registry.example/database@sha256:abc".to_owned(),
                    command: vec!["database".to_owned()],
                    arguments: Vec::new(),
                    environment: BTreeMap::new(),
                    reconfigurable_environment: BTreeSet::new(),
                    storage: vec![WorkloadStorageAttachment {
                        name: "data".to_owned(),
                        destination: "/var/lib/database".to_owned(),
                        kind: super::super::WorkloadStorageKind::Data,
                        namespace: "deployment".to_owned(),
                        key: "group-one".to_owned(),
                        read_only: false,
                        expires_when_unused: false,
                    }],
                    ports: vec![WorkloadPort {
                        name: "http".to_owned(),
                        container_port: 8080,
                    }],
                    dependencies: Vec::new(),
                    ready_launch_gate: Some(gate.clone()),
                },
            ),
            (
                WorkloadUnitName("web".to_owned()),
                WorkloadUnitLaunch {
                    image: "registry.example/web@sha256:def".to_owned(),
                    command: vec!["web".to_owned()],
                    arguments: vec!["--listen=8080".to_owned()],
                    environment: BTreeMap::from([("MODE".to_owned(), "poc".to_owned())]),
                    reconfigurable_environment: BTreeSet::new(),
                    storage: Vec::new(),
                    ports: vec![WorkloadPort {
                        name: "http".to_owned(),
                        container_port: 8080,
                    }],
                    dependencies: vec![
                        WorkloadUnitDependency {
                            unit_name: WorkloadUnitName("cache".to_owned()),
                            condition: WorkloadDependencyCondition::Started,
                        },
                        WorkloadUnitDependency {
                            unit_name: WorkloadUnitName("database".to_owned()),
                            condition: WorkloadDependencyCondition::Ready,
                        },
                    ],
                    ready_launch_gate: None,
                },
            ),
        ]);
        let launch = WorkloadGroupLaunch {
            group_id: WorkloadGroupId("open-webui-01".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "application-deployment-01".to_owned(),
                reconcile_generation: 7,
            },
            execution_target: ExecutionTarget {
                managed_node_id: SiteNodeId::for_test("node-a"),
                accelerator_device_ids: Vec::new(),
            },
            execution_target_id: "application-target-a".to_owned(),
            required_platform: dotcore_domain::ExecutionPlatform::linux_amd64(),
            units,
            publication: WorkloadPublicationRequest {
                unit_name: WorkloadUnitName("web".to_owned()),
                port_name: "http".to_owned(),
                configured_host: Ipv4Addr::new(10, 20, 30, 40),
                host_port: NonZeroU16::new(30_000).expect("nonzero port"),
            },
            host_aliases: Default::default(),
        };

        assert_eq!(launch.group_id.0, "open-webui-01");
        assert_eq!(launch.correlation.owner_id, "application-deployment-01");
        assert_eq!(launch.units.len(), 3);
        assert_eq!(
            launch.units[&WorkloadUnitName("web".to_owned())].dependencies,
            vec![
                WorkloadUnitDependency {
                    unit_name: WorkloadUnitName("cache".to_owned()),
                    condition: WorkloadDependencyCondition::Started,
                },
                WorkloadUnitDependency {
                    unit_name: WorkloadUnitName("database".to_owned()),
                    condition: WorkloadDependencyCondition::Ready,
                },
            ]
        );
        assert_eq!(
            launch.units[&WorkloadUnitName("database".to_owned())].ready_launch_gate,
            Some(gate)
        );
        assert_eq!(
            launch.publication.configured_host,
            Ipv4Addr::new(10, 20, 30, 40)
        );
    }

    #[test]
    fn ready_launch_gate_enforces_bounded_portable_input() {
        let gate = ReadyLaunchGate::http_get(
            "http",
            "/ready",
            MAX_READY_LAUNCH_GATE_TIMEOUT,
            Duration::from_secs(1),
        )
        .expect("maximum bounded gate");

        assert_eq!(gate.port_name(), "http");
        assert_eq!(gate.path(), "/ready");
        assert_eq!(gate.timeout(), MAX_READY_LAUNCH_GATE_TIMEOUT);
        assert_eq!(gate.interval(), Duration::from_secs(1));
        assert_eq!(
            ReadyLaunchGateError::InvalidPortName.to_string(),
            "READY launch-gate port name is invalid"
        );
        assert_eq!(
            ReadyLaunchGateError::InvalidHttpPath.to_string(),
            "READY launch-gate HTTP path is invalid"
        );
        assert_eq!(
            ReadyLaunchGateError::InvalidTimeout.to_string(),
            "READY launch-gate timeout is invalid"
        );
        assert_eq!(
            ReadyLaunchGateError::InvalidInterval.to_string(),
            "READY launch-gate interval is invalid"
        );
        assert!(std::error::Error::source(&ReadyLaunchGateError::InvalidTimeout).is_none());
        assert_eq!(
            ReadyLaunchGate::http_get("http", "/ready", Duration::ZERO, Duration::from_secs(1),),
            Err(ReadyLaunchGateError::InvalidTimeout)
        );
        assert_eq!(
            ReadyLaunchGate::http_get(
                "http",
                "/ready",
                MAX_READY_LAUNCH_GATE_TIMEOUT + Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(ReadyLaunchGateError::InvalidTimeout)
        );
        assert_eq!(
            ReadyLaunchGate::http_get(
                "http",
                "/ready",
                Duration::from_secs(5),
                Duration::from_secs(6),
            ),
            Err(ReadyLaunchGateError::InvalidInterval)
        );
        assert_eq!(
            ReadyLaunchGate::http_get(
                "http",
                "ready",
                Duration::from_secs(5),
                Duration::from_secs(1),
            ),
            Err(ReadyLaunchGateError::InvalidHttpPath)
        );
        let oversized_path = format!("/{}", "x".repeat(MAX_READY_LAUNCH_GATE_PATH_BYTES));
        assert_eq!(
            ReadyLaunchGate::http_get(
                "http",
                oversized_path,
                Duration::from_secs(5),
                Duration::from_secs(1),
            ),
            Err(ReadyLaunchGateError::InvalidHttpPath)
        );
    }

    #[test]
    fn workload_group_observation_reports_units_endpoint_and_safe_stale_reason() {
        let group = ObservedWorkloadGroup {
            group_id: WorkloadGroupId("open-webui-01".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "application-deployment-01".to_owned(),
                reconcile_generation: 7,
            },
            backend_id: RuntimeBackendId("runtime-a".to_owned()),
            units: BTreeMap::from([
                (
                    WorkloadUnitName("database".to_owned()),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Running,
                        ready_launch_gate: Some(ReadyLaunchGateOutcome::Passed),
                        host_process: None,
                    },
                ),
                (
                    WorkloadUnitName("web".to_owned()),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Starting,
                        ready_launch_gate: Some(ReadyLaunchGateOutcome::TimedOut),
                        host_process: None,
                    },
                ),
                (
                    WorkloadUnitName("worker".to_owned()),
                    WorkloadUnitObservation {
                        state: WorkloadUnitState::Stopped,
                        ready_launch_gate: Some(ReadyLaunchGateOutcome::Failed(
                            ReadyLaunchGateFailure::ConnectionFailed,
                        )),
                        host_process: None,
                    },
                ),
            ]),
            assigned_endpoint: Some(AssignedWorkloadEndpoint {
                unit_name: WorkloadUnitName("web".to_owned()),
                port_name: "http".to_owned(),
                configured_host: Ipv4Addr::new(10, 20, 30, 40),
                assigned_port: NonZeroU16::new(32_768).expect("non-zero port"),
            }),
        };
        let observation = WorkloadGroupObservation::StaleConfiguration {
            group: group.clone(),
            reason: WorkloadGroupStaleReason::PublicationHostChanged,
        };

        assert_eq!(
            observation,
            WorkloadGroupObservation::StaleConfiguration {
                group,
                reason: WorkloadGroupStaleReason::PublicationHostChanged,
            }
        );
        assert_eq!(
            WorkloadGroupStaleReason::PublicationHostChanged.as_str(),
            "configured publication host changed"
        );
        assert!(
            WorkloadGroupStaleReason::PublicationHostChanged
                .as_str()
                .len()
                <= MAX_WORKLOAD_GROUP_STALE_REASON_BYTES
        );
        let WorkloadGroupObservation::StaleConfiguration { group, .. } = observation else {
            panic!("stale observation");
        };
        assert_eq!(
            group.units[&WorkloadUnitName("web".to_owned())].ready_launch_gate,
            Some(ReadyLaunchGateOutcome::TimedOut)
        );
        assert_eq!(
            group.units[&WorkloadUnitName("worker".to_owned())].ready_launch_gate,
            Some(ReadyLaunchGateOutcome::Failed(
                ReadyLaunchGateFailure::ConnectionFailed
            ))
        );
    }
}
