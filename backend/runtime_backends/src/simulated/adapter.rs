//! In-memory runtime adapter for execution-target lifecycle behavior.

use std::{
    collections::BTreeMap,
    num::NonZeroU16,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{ExecutionPlatform, host_process::HostProcessIdentity};
use dotcore_runtime::{
    AssignedWorkloadEndpoint, ObservedWorkload, ObservedWorkloadGroup, RuntimeBackend,
    RuntimeBackendError, RuntimeBackendId, RuntimeFuture, RuntimeGroupLogFollowQuery,
    RuntimeLogFuture, RuntimeLogHistory, RuntimeLogQuery, RuntimeLogSourceHistory,
    RuntimeLogSourceSelection, RuntimeLogSourceStream, RuntimeLogStream,
    RuntimeWorkloadLogFollowQuery, RuntimeWorkloadLogQuery, WorkloadCorrelation,
    WorkloadGroupBackend, WorkloadGroupId, WorkloadGroupLaunch, WorkloadGroupObservation,
    WorkloadId, WorkloadLaunch, WorkloadMaterializationRequirements, WorkloadObservation,
    WorkloadServiceBinding, WorkloadStartupActivity, WorkloadStartupObserver, WorkloadStartupPhase,
    WorkloadStartupProgress, WorkloadStartupProgressUnit, WorkloadStartupRequest,
    WorkloadStorageAttachment, WorkloadStorageKey, WorkloadUnitName, WorkloadUnitObservation,
    WorkloadUnitResourceObservation, WorkloadUnitResources, WorkloadUnitResourcesUnavailable,
    WorkloadUnitState,
};
use tokio_util::sync::CancellationToken;

use super::configuration::{
    SimulatedRuntimeConfig, SimulatedRuntimeGroupCall, SimulatedRuntimeGroupDrift,
    SimulatedRuntimeTestSupport,
};
use super::machine::ApplicationUnitProcess;

/// Runtime backend identifier of the simulated backend.
pub const BACKEND_ID: &str = "runtime/simulated-memory";

/// In-memory runtime backend with execution-target lifecycle scenarios.
#[derive(Debug)]
pub struct InMemoryRuntimeBackend {
    config: SimulatedRuntimeConfig,
    pub(super) state: Arc<Mutex<FakeState>>,
}

impl InMemoryRuntimeBackend {
    /// Creates a deterministic backend for an explicit effective execution platform.
    pub fn new(execution_platform: ExecutionPlatform) -> Self {
        Self::with_config(SimulatedRuntimeConfig {
            execution_platform,
            ..Default::default()
        })
    }

    /// Creates a deterministic backend with its own workloads and the given lifecycle behavior.
    pub fn with_config(config: SimulatedRuntimeConfig) -> Self {
        Self {
            config,
            state: Arc::new(Mutex::new(FakeState::default())),
        }
    }

    /// Creates the backend of one execution target. Test support that retains workloads hands
    /// every backend of the same target the same workloads, as a runtime that outlives a backend
    /// restart keeps them.
    pub fn for_target(config: SimulatedRuntimeConfig, execution_target_id: &str) -> Self {
        let state = config
            .test_support
            .as_ref()
            .and_then(|support| support.retained_workloads(execution_target_id))
            .unwrap_or_default();
        Self { config, state }
    }

    fn ready_delay_for(&self, deployment_id: &str) -> Duration {
        self.config
            .test_support
            .as_ref()
            .and_then(SimulatedRuntimeTestSupport::lifecycle_delays)
            .map(|(ready, _)| ready)
            .or_else(|| {
                self.config
                    .deployment_ready_delays
                    .get(deployment_id)
                    .copied()
            })
            .unwrap_or(self.config.default_ready_delay)
    }

    fn stop_delay_for(&self, deployment_id: &str) -> Duration {
        self.config
            .test_support
            .as_ref()
            .and_then(SimulatedRuntimeTestSupport::lifecycle_delays)
            .map(|(_, stop)| stop)
            .or_else(|| {
                self.config
                    .deployment_stop_delays
                    .get(deployment_id)
                    .copied()
            })
            .unwrap_or(self.config.default_stop_delay)
    }

    fn observation(&self, workload: &mut FakeWorkload) -> WorkloadObservation {
        if workload.state == FakeWorkloadState::Loading
            && self.config.clock.now() >= workload.transition_at
            && self
                .config
                .test_support
                .as_ref()
                .is_none_or(SimulatedRuntimeTestSupport::readiness_released)
        {
            workload.state = FakeWorkloadState::Ready;
        }
        if workload.state == FakeWorkloadState::Unloading
            && self.config.clock.now() >= workload.transition_at
            && self
                .config
                .test_support
                .as_ref()
                .is_none_or(SimulatedRuntimeTestSupport::stop_released)
        {
            workload.state = FakeWorkloadState::Stopped;
        }
        match workload.state {
            FakeWorkloadState::Loading => WorkloadObservation::Starting(workload.identity.clone()),
            FakeWorkloadState::Ready => WorkloadObservation::Running(workload.identity.clone()),
            FakeWorkloadState::Unloading => {
                WorkloadObservation::Stopping(workload.identity.clone())
            }
            FakeWorkloadState::Stopped => WorkloadObservation::stopped(workload.identity.clone()),
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct FakeState {
    pub(super) next_workload: u64,
    pub(super) workloads: BTreeMap<String, FakeWorkload>,
    pub(super) absence_confirmation: Option<ObservedWorkload>,
    groups: BTreeMap<String, FakeWorkloadGroup>,
}

#[derive(Debug)]
struct FakeWorkloadGroup {
    launch: WorkloadGroupLaunch,
    unit_states: BTreeMap<WorkloadUnitName, WorkloadUnitState>,

    /// Simulated process of every running unit on the node's declared machine, if any.
    unit_processes: BTreeMap<WorkloadUnitName, ApplicationUnitProcess>,
}

impl WorkloadGroupBackend for InMemoryRuntimeBackend {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(BACKEND_ID.to_owned())
    }

    fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
        WorkloadMaterializationRequirements::virtualized()
    }

    fn execution_platform<'a>(
        &'a self,
        _execution_target_id: &'a str,
    ) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(self
            .config
            .execution_platform
            .clone())))
    }

    fn discover_workload_groups<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadGroupObservation>> {
        Box::pin(async move {
            let state = self.state.lock().expect("simulated runtime state");
            Ok(state
                .groups
                .values()
                .filter_map(|group| {
                    let observation = simulated_group_observation(
                        group,
                        RuntimeBackend::backend_id(self),
                        self.config
                            .test_support
                            .as_ref()
                            .and_then(SimulatedRuntimeTestSupport::application_endpoint),
                    );
                    match self
                        .config
                        .test_support
                        .as_ref()
                        .and_then(|support| support.group_drift(&group.launch.group_id.0))
                    {
                        Some(SimulatedRuntimeGroupDrift::Absent) => None,
                        drift => Some(apply_group_drift(observation, drift)),
                    }
                })
                .collect())
        })
    }

    fn storage_exists<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, bool> {
        Box::pin(std::future::ready(Ok(self
            .config
            .test_support
            .as_ref()
            .is_some_and(|support| support.storage_exists(storage)))))
    }

    fn remove_storage<'a>(&'a self, storage: &'a WorkloadStorageKey) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            if let Some(test_support) = &self.config.test_support {
                test_support.record_group_call("remove-storage", &storage.key);
                test_support.remove_storage(storage);
            }
            Ok(())
        })
    }

    fn ensure_workload_group_absent<'a>(
        &'a self,
        group_id: &'a WorkloadGroupId,
        owner_id: &'a str,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            if !dotcore_domain::valid_id(&group_id.0) || !dotcore_domain::valid_id(owner_id) {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group cleanup identity is invalid",
                ));
            }
            let mut state = self.state.lock().expect("simulated runtime state");
            if let Some(stored) = state.groups.get(&group_id.0)
                && stored.launch.correlation.owner_id != owner_id
            {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group belongs to another owner",
                ));
            }
            if let Some(test_support) = &self.config.test_support {
                test_support.record_group_call("ensure-absent", &group_id.0);
            }
            self.forget_group(&mut state, &group_id.0);
            Ok(())
        })
    }

    fn create_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            validate_group_platform(group, &self.config.execution_platform)?;
            let mut state = self.state.lock().expect("simulated runtime state");
            if let Some(test_support) = &self.config.test_support {
                let mut support = test_support
                    .state
                    .lock()
                    .expect("simulated runtime test support");
                support
                    .group_calls
                    .push(SimulatedRuntimeGroupCall::create(&group.group_id.0));
                support.group_launches.push(group.clone());
                drop(support);
                test_support.record_storage(
                    group
                        .units
                        .values()
                        .flat_map(|unit| &unit.storage)
                        .map(WorkloadStorageAttachment::storage_key),
                );
            }
            if !state.groups.contains_key(&group.group_id.0) {
                let mut created = FakeWorkloadGroup {
                    launch: group.clone(),
                    unit_states: group
                        .units
                        .keys()
                        .cloned()
                        .map(|name| (name, WorkloadUnitState::Running))
                        .collect(),
                    unit_processes: BTreeMap::new(),
                };
                self.start_unit_processes(&mut created);
                state.groups.insert(group.group_id.0.clone(), created);
            }
            Ok(simulated_group_observation(
                state
                    .groups
                    .get(&group.group_id.0)
                    .expect("inserted simulated group"),
                RuntimeBackend::backend_id(self),
                self.config
                    .test_support
                    .as_ref()
                    .and_then(SimulatedRuntimeTestSupport::application_endpoint),
            ))
        })
    }

    fn start_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            validate_group_platform(group, &self.config.execution_platform)?;
            let mut state = self.state.lock().expect("simulated runtime state");
            if self
                .config
                .test_support
                .as_ref()
                .is_some_and(|support| support.record_group_call("start", &group.group_id.0))
            {
                return Err(RuntimeBackendError::invalid_launch(
                    "injected simulated runtime group start failure",
                ));
            }
            let Some(stored) = state.groups.get_mut(&group.group_id.0) else {
                return Ok(WorkloadGroupObservation::Absent {
                    group_id: group.group_id.clone(),
                    correlation: group.correlation.clone(),
                    backend_id: RuntimeBackend::backend_id(self),
                });
            };
            if stored.launch != *group {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group launch identity does not match start request",
                ));
            }
            for unit in stored.unit_states.values_mut() {
                *unit = WorkloadUnitState::Running;
            }
            self.start_unit_processes(stored);
            Ok(simulated_group_observation(
                stored,
                RuntimeBackend::backend_id(self),
                self.config
                    .test_support
                    .as_ref()
                    .and_then(SimulatedRuntimeTestSupport::application_endpoint),
            ))
        })
    }

    fn inspect_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, WorkloadGroupObservation> {
        Box::pin(async move {
            if let Some(test_support) = &self.config.test_support {
                test_support
                    .state
                    .lock()
                    .expect("simulated runtime test support")
                    .group_calls
                    .push(SimulatedRuntimeGroupCall::inspect(&group.group_id.0));
            }
            let state = self.state.lock().expect("simulated runtime state");
            Ok(match state.groups.get(&group.group_id.0) {
                Some(existing) => {
                    let observation = simulated_group_observation(
                        existing,
                        RuntimeBackend::backend_id(self),
                        self.config
                            .test_support
                            .as_ref()
                            .and_then(SimulatedRuntimeTestSupport::application_endpoint),
                    );
                    apply_group_drift(
                        observation,
                        self.config
                            .test_support
                            .as_ref()
                            .and_then(|support| support.group_drift(&group.group_id.0)),
                    )
                }
                None => WorkloadGroupObservation::Absent {
                    group_id: group.group_id.clone(),
                    correlation: group.correlation.clone(),
                    backend_id: RuntimeBackend::backend_id(self),
                },
            })
        })
    }

    fn observe_group_resources<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation>> {
        Box::pin(std::future::ready(Ok(self.group_resources(group))))
    }

    fn remove_workload_group<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("simulated runtime state");
            if let Some(test_support) = &self.config.test_support {
                test_support
                    .state
                    .lock()
                    .expect("simulated runtime test support")
                    .group_calls
                    .push(SimulatedRuntimeGroupCall::new("remove", &group.group_id.0));
            }
            if let Some(stored) = state.groups.get(&group.group_id.0)
                && stored.launch != *group
            {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group launch identity does not match removal request",
                ));
            }
            self.forget_group(&mut state, &group.group_id.0);
            Ok(())
        })
    }

    fn remove_observed_workload_group<'a>(
        &'a self,
        group: &'a ObservedWorkloadGroup,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("simulated runtime state");
            let Some(stored) = state.groups.get(&group.group_id.0) else {
                return Ok(());
            };
            if group.backend_id != RuntimeBackend::backend_id(self)
                || stored.launch.correlation != group.correlation
                || stored.unit_states.keys().ne(group.units.keys())
            {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated observed workload group is not exact owned infrastructure",
                ));
            }
            if let Some(test_support) = &self.config.test_support {
                test_support.record_group_call("remove", &group.group_id.0);
            }
            self.forget_group(&mut state, &group.group_id.0);
            Ok(())
        })
    }

    fn stop_workload_group<'a>(&'a self, group: &'a WorkloadGroupLaunch) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            let mut state = self.state.lock().expect("simulated runtime state");
            if self
                .config
                .test_support
                .as_ref()
                .is_some_and(|support| support.record_group_call("stop", &group.group_id.0))
            {
                return Err(RuntimeBackendError::invalid_launch(
                    "injected simulated runtime group stop failure",
                ));
            }
            let Some(stored) = state.groups.get_mut(&group.group_id.0) else {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group is unknown",
                ));
            };
            if stored.launch != *group {
                return Err(RuntimeBackendError::invalid_launch(
                    "simulated workload group launch identity does not match stop request",
                ));
            }
            for unit in stored.unit_states.values_mut() {
                *unit = WorkloadUnitState::Stopped;
            }
            self.end_unit_processes(stored);
            Ok(())
        })
    }

    fn read_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: &'a RuntimeLogQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceHistory>> {
        Box::pin(std::future::ready(
            self.read_group_logs(group, sources, query),
        ))
    }

    fn follow_workload_group_logs<'a>(
        &'a self,
        group: &'a WorkloadGroupLaunch,
        sources: &'a RuntimeLogSourceSelection,
        query: RuntimeGroupLogFollowQuery,
    ) -> RuntimeLogFuture<'a, Vec<RuntimeLogSourceStream>> {
        Box::pin(std::future::ready(
            self.follow_group_logs(group, sources, query),
        ))
    }
}

impl InMemoryRuntimeBackend {
    /// Synthetic figures of every requested unit, from its process on the declared machine. A
    /// unit without a running process, or of another launch generation, has none; without a
    /// machine, the runtime reports none.
    fn group_resources(
        &self,
        group: &WorkloadGroupLaunch,
    ) -> BTreeMap<WorkloadUnitName, WorkloadUnitResourceObservation> {
        let Some(machine) = &self.config.machine else {
            let observed_at = std::time::SystemTime::now();
            return group
                .units
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        WorkloadUnitResourceObservation {
                            observed_at,
                            resources: WorkloadUnitResources::NotReported,
                        },
                    )
                })
                .collect();
        };
        let state = self.state.lock().expect("simulated runtime state");
        let stored = state
            .groups
            .get(&group.group_id.0)
            .filter(|stored| stored.launch.correlation == group.correlation);
        group
            .units
            .keys()
            .map(|name| {
                let observation = match stored.and_then(|stored| stored.unit_processes.get(name)) {
                    Some(process) => machine.application_unit_resources(process),
                    None => WorkloadUnitResourceObservation {
                        observed_at: machine.wall_now(),
                        resources: WorkloadUnitResources::Unavailable(
                            WorkloadUnitResourcesUnavailable::NotRunning,
                        ),
                    },
                };
                (name.clone(), observation)
            })
            .collect()
    }

    /// Starts a process on the declared machine for every unit of `group` without one, so its
    /// synthetic memory counts as used there.
    fn start_unit_processes(&self, group: &mut FakeWorkloadGroup) {
        let Some((machine, node)) = self
            .config
            .machine
            .as_ref()
            .zip(self.config.managed_node_id.as_ref())
        else {
            return;
        };
        for name in group.unit_states.keys() {
            group
                .unit_processes
                .entry(name.clone())
                .or_insert_with(|| machine.start_application_unit(node));
        }
    }

    /// Ends every unit process of `group`, releasing its memory on the declared machine.
    fn end_unit_processes(&self, group: &mut FakeWorkloadGroup) {
        let processes = std::mem::take(&mut group.unit_processes);
        if let Some(machine) = &self.config.machine {
            processes
                .values()
                .for_each(|process| machine.end_application_unit(process));
        }
    }

    /// Drops a retained group and ends its unit processes.
    fn forget_group(&self, state: &mut FakeState, group_id: &str) {
        if let Some(mut removed) = state.groups.remove(group_id) {
            self.end_unit_processes(&mut removed);
        }
    }

    /// Whether exactly `group` is retained. A group retained under the same identity with another
    /// launch is an error, as for every other exact group operation.
    pub(super) fn retains_group(
        &self,
        group: &WorkloadGroupLaunch,
    ) -> Result<bool, RuntimeBackendError> {
        let state = self.state.lock().expect("simulated runtime state");
        match state.groups.get(&group.group_id.0) {
            None => Ok(false),
            Some(stored) if stored.launch == *group => Ok(true),
            Some(_) => Err(RuntimeBackendError::invalid_launch(
                "simulated workload group launch identity does not match log request",
            )),
        }
    }
}

fn validate_group_platform(
    group: &WorkloadGroupLaunch,
    observed: &dotcore_domain::ExecutionPlatform,
) -> Result<(), RuntimeBackendError> {
    if &group.required_platform != observed {
        return Err(RuntimeBackendError::TargetPlatformMismatch {
            required_operating_system: group.required_platform.operating_system().to_owned(),
            required_architecture: group.required_platform.architecture().to_owned(),
            observed_operating_system: observed.operating_system().to_owned(),
            observed_architecture: observed.architecture().to_owned(),
        });
    }
    Ok(())
}

fn simulated_group_observation(
    group: &FakeWorkloadGroup,
    backend_id: RuntimeBackendId,
    endpoint_port: Option<NonZeroU16>,
) -> WorkloadGroupObservation {
    WorkloadGroupObservation::Observed(ObservedWorkloadGroup {
        group_id: group.launch.group_id.clone(),
        correlation: group.launch.correlation.clone(),
        backend_id,
        units: group
            .unit_states
            .iter()
            .map(|(unit, state)| {
                (
                    unit.clone(),
                    WorkloadUnitObservation {
                        state: *state,
                        ready_launch_gate: None,
                        host_process: None,
                    },
                )
            })
            .collect(),
        // Publishes on the requested port, as a real runtime would, unless test support assigns
        // the port of a server it runs.
        assigned_endpoint: Some(AssignedWorkloadEndpoint {
            unit_name: group.launch.publication.unit_name.clone(),
            port_name: group.launch.publication.port_name.clone(),
            configured_host: group.launch.publication.configured_host,
            assigned_port: endpoint_port.unwrap_or(group.launch.publication.host_port),
        }),
    })
}

fn apply_group_drift(
    observation: WorkloadGroupObservation,
    drift: Option<SimulatedRuntimeGroupDrift>,
) -> WorkloadGroupObservation {
    let Some(drift) = drift else {
        return observation;
    };
    let WorkloadGroupObservation::Observed(mut group) = observation else {
        return observation;
    };
    match drift {
        SimulatedRuntimeGroupDrift::Absent => WorkloadGroupObservation::Absent {
            group_id: group.group_id,
            correlation: group.correlation,
            backend_id: group.backend_id,
        },
        SimulatedRuntimeGroupDrift::Partial => {
            group.units.pop_first();
            WorkloadGroupObservation::Observed(group)
        }
        SimulatedRuntimeGroupDrift::Stale => WorkloadGroupObservation::StaleConfiguration {
            group,
            reason: dotcore_runtime::WorkloadGroupStaleReason::PublicationHostChanged,
        },
    }
}

#[derive(Clone, Debug)]
pub(super) struct FakeWorkload {
    pub(super) identity: ObservedWorkload,

    /// Simulated process taking the workload's memory on the node's declared machine, if any.
    pub(super) machine_process: Option<HostProcessIdentity>,

    pub(super) transition_at: tokio::time::Instant,
    pub(super) state: FakeWorkloadState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FakeWorkloadState {
    Loading,
    Ready,
    Unloading,
    Stopped,
}

impl RuntimeBackend for InMemoryRuntimeBackend {
    fn backend_id(&self) -> RuntimeBackendId {
        RuntimeBackendId(BACKEND_ID.to_owned())
    }

    fn execution_platform<'a>(&'a self) -> RuntimeFuture<'a, ExecutionPlatform> {
        Box::pin(std::future::ready(Ok(self
            .config
            .execution_platform
            .clone())))
    }

    fn materialization_requirements(&self) -> WorkloadMaterializationRequirements {
        WorkloadMaterializationRequirements::virtualized()
    }

    // A simulated workload runs no process, so it has nothing to share; accepting the intent lets
    // engines that need host IPC, such as vLLM, run on a declared machine as on the real one.
    fn supports_host_shared_ipc(&self) -> bool {
        true
    }

    fn prepare_realization<'a>(
        &'a self,
        request: &'a WorkloadStartupRequest,
        cancellation: CancellationToken,
        observer: Option<WorkloadStartupObserver>,
    ) -> RuntimeFuture<'a, ()> {
        Box::pin(async move {
            if request.provenance != dotcore_runtime::WorkloadRealizationProvenance::FirstActivation
            {
                return Ok(());
            }
            let Some(startup) = &self.config.startup else {
                return Ok(());
            };
            for &completed in startup.completed_steps.iter() {
                if cancellation.is_cancelled() {
                    return Err(RuntimeBackendError::preparation_cancelled());
                }
                if let Some(observer) = &observer {
                    observer(WorkloadStartupActivity {
                        phase: WorkloadStartupPhase::AcquiringAssets,
                        progress: Some(WorkloadStartupProgress {
                            completed,
                            total: Some(startup.total),
                            unit: WorkloadStartupProgressUnit::Bytes,
                        }),
                        detail: Some("Simulating model artifact download".to_owned()),
                    });
                }
                if let Some(test_support) = &self.config.test_support
                    && test_support.hold_preparation(&cancellation).await.is_err()
                {
                    return Err(RuntimeBackendError::preparation_cancelled());
                }
                tokio::select! {
                    biased;
                    () = cancellation.cancelled() => {
                        return Err(RuntimeBackendError::preparation_cancelled());
                    }
                    () = self.config.clock.sleep_until(
                        self.config.clock.now() + startup.step_duration
                    ) => {}
                }
            }
            Ok(())
        })
    }

    fn discover_workloads<'a>(&'a self) -> RuntimeFuture<'a, Vec<WorkloadObservation>> {
        if self
            .config
            .test_support
            .as_ref()
            .is_some_and(SimulatedRuntimeTestSupport::record_discovery)
        {
            return Box::pin(std::future::ready(Err(
                RuntimeBackendError::invalid_launch("injected simulated runtime discovery failure"),
            )));
        }
        Box::pin(async {
            Ok(self
                .state
                .lock()
                .expect("simulated runtime state")
                .workloads
                .values_mut()
                .map(|workload| self.observation(workload))
                .collect())
        })
    }

    fn create_workload<'a>(
        &'a self,
        workload: &'a WorkloadLaunch,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        if let Err(error) = workload
            .spec
            .validate_runtime_platform(&self.config.execution_platform)
        {
            return Box::pin(std::future::ready(Err(error)));
        }

        if let Err(error) = workload.validate_backend_capabilities(self) {
            return Box::pin(std::future::ready(Err(error)));
        }
        let mut state = self.state.lock().expect("simulated runtime state");
        if let Some(test_support) = &self.config.test_support {
            test_support.record_create();
        }
        if let Some(existing) = state.workloads.values_mut().find(|existing| {
            existing.identity.correlation.owner_id == workload.correlation.owner_id
        }) {
            return Box::pin(std::future::ready(Ok(self.observation(existing))));
        }
        // On a declared machine, the workload is a simulated process taking its expected memory.
        let machine_process = match self
            .config
            .machine
            .as_ref()
            .zip(self.config.managed_node_id.as_ref())
        {
            Some((machine, node)) => match machine
                .start_workload(node, &workload.spec.expected_memory)
            {
                Ok(identity) => Some(identity),
                Err(error) => {
                    return Box::pin(std::future::ready(Err(RuntimeBackendError::InvalidLaunch(
                        anyhow::Error::new(error)
                            .context("simulated workload memory names a GPU the machine lacks"),
                    ))));
                }
            },
            None => None,
        };
        // Like a real process, only one holding GPU memory is observed on an accelerator, so only
        // then does memory attribution relate the workload to it.
        let host_process = machine_process
            .clone()
            .filter(|_| !workload.spec.expected_memory.device_bytes.is_empty());
        state.next_workload += 1;
        let workload_id = WorkloadId(format!("simulated-workload-{}", state.next_workload));
        let identity = ObservedWorkload {
            workload_id: workload_id.clone(),
            correlation: workload.correlation.clone(),
            backend_id: RuntimeBackend::backend_id(self),
            loopback_endpoints: Vec::new(),
            services: vec![WorkloadServiceBinding::native(
                workload_id.0,
                "http",
                workload.correlation.reconcile_generation,
            )],
            host_process,
        };
        let delay = self.ready_delay_for(&workload.correlation.owner_id);
        let mut created = FakeWorkload {
            identity: identity.clone(),
            machine_process,
            transition_at: self.config.clock.now() + delay,
            state: FakeWorkloadState::Loading,
        };
        let observation = self.observation(&mut created);
        state
            .workloads
            .insert(identity.workload_id.0.clone(), created);
        Box::pin(std::future::ready(Ok(observation)))
    }

    fn start_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        let mut state = self.state.lock().expect("simulated runtime state");
        if let Some(test_support) = &self.config.test_support {
            test_support.record_start();
        }
        let result = state
            .workloads
            .get_mut(&workload_id.0)
            .map(|workload| {
                workload.state = FakeWorkloadState::Loading;
                workload.transition_at = self.config.clock.now()
                    + self.ready_delay_for(&workload.identity.correlation.owner_id);
                self.observation(workload)
            })
            .ok_or_else(|| RuntimeBackendError::invalid_launch("simulated workload is unknown"));
        Box::pin(std::future::ready(result))
    }

    fn inspect_workload<'a>(
        &'a self,
        workload_id: &'a WorkloadId,
    ) -> RuntimeFuture<'a, WorkloadObservation> {
        let mut state = self.state.lock().expect("simulated runtime state");
        Box::pin(std::future::ready(
            match state.workloads.get_mut(&workload_id.0) {
                Some(workload) => Ok(self.observation(workload)),
                None => state
                    .absence_confirmation
                    .as_ref()
                    .filter(|workload| workload.workload_id == *workload_id)
                    .cloned()
                    .map(WorkloadObservation::Absent)
                    .ok_or_else(|| {
                        RuntimeBackendError::invalid_launch("simulated workload is unknown")
                    }),
            },
        ))
    }

    fn stop_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        let mut state = self.state.lock().expect("simulated runtime state");
        let result = match state.workloads.get_mut(&workload_id.0) {
            Some(_)
                if self
                    .config
                    .test_support
                    .as_ref()
                    .is_some_and(SimulatedRuntimeTestSupport::record_stop) =>
            {
                Err(RuntimeBackendError::invalid_launch(
                    "injected simulated runtime stop failure",
                ))
            }
            Some(workload) => {
                if !matches!(
                    workload.state,
                    FakeWorkloadState::Unloading | FakeWorkloadState::Stopped
                ) {
                    workload.state = FakeWorkloadState::Unloading;
                    workload.transition_at = self.config.clock.now()
                        + self.stop_delay_for(&workload.identity.correlation.owner_id);
                }
                Ok(())
            }
            None => Err(RuntimeBackendError::invalid_launch(
                "simulated workload is unknown",
            )),
        };
        Box::pin(std::future::ready(result))
    }

    fn remove_workload<'a>(&'a self, workload_id: &'a WorkloadId) -> RuntimeFuture<'a, ()> {
        let mut state = self.state.lock().expect("simulated runtime state");
        if let Some(workload) = state.workloads.remove(&workload_id.0) {
            if let Some((machine, process)) = self
                .config
                .machine
                .as_ref()
                .zip(workload.machine_process.as_ref())
            {
                machine.end_workload(process);
            }
            // Controller operations serialize removal and its immediate inspection.
            // One retained identity provides that absence confirmation without growth.
            state.absence_confirmation = Some(workload.identity);
        }
        Box::pin(std::future::ready(Ok(())))
    }

    fn read_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: &'a RuntimeWorkloadLogQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogHistory> {
        Box::pin(std::future::ready(
            self.read_logs_by_correlation(correlation, query),
        ))
    }

    fn follow_workload_logs_by_correlation<'a>(
        &'a self,
        correlation: &'a WorkloadCorrelation,
        query: RuntimeWorkloadLogFollowQuery,
    ) -> RuntimeLogFuture<'a, RuntimeLogStream> {
        Box::pin(std::future::ready(
            self.follow_logs_by_correlation(correlation, query),
        ))
    }
}
