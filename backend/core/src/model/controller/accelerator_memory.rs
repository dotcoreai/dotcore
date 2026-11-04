//! Projects current accelerator-memory usage for model deployments from in-memory observations.
//!
//! The hosting node attributes usage from its own process observations, or from the workload
//! process memory its runtime reports (the native runtime on unified-memory targets), and reports
//! the result in deployment status; the owner projects that reported result for node-hosted
//! placements. Runtime-reported memory is read with each hardware collection from the first time
//! the workload runs, so a status attributes it from the same collection as everything else.
//! Observation-based attribution requires exact node-qualified process lifetime identity, and
//! unavailable evidence never falls back to device-wide memory or a false zero.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use dotcore_domain::{ResolvedAccelerators, host_process::HostProcessIdentity};
use dotcore_proto::{
    dotcore::{
        deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation, AcceleratorMemoryUsageSet,
            accelerator_memory_observation,
        },
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentState, ModelDeploymentStatus,
            model_deployment_snapshot,
        },
        site::v1::ExecutionTarget,
    },
    google::protobuf::Timestamp,
};
use dotcore_runtime::{
    ObservedWorkload, ProcessMemory, ProcessMemoryObservation, ProcessMemoryUnavailable,
    WorkloadId, WorkloadObservation,
};
use dotcore_site::node::{
    agent::CollectionReading,
    hardware_inventory::{HardwareCollection, HardwareInventorySource},
};
use tracing::debug;

use super::resource_telemetry::{ReadyRealization, WorkloadTelemetry};
use crate::{
    dispatch::model::RuntimeTargetMap,
    hardware::{
        accelerator_presence,
        attribution::{
            AttributionScope, AttributionSource, PROCESS_MEMORY_PENDING, attribution_scope,
            node_status_unavailable, not_applicable, project_identities, unavailable,
        },
    },
    telemetry::TelemetrySink,
};

/// Attributes accelerator memory to the deployments realized in this process and samples their
/// resource telemetry. Clones share the same process identities, inventory, and runtime-reported
/// readings, so the realizer that observes workloads, the collection that reads their memory, and
/// the status reporter that projects them use one view.
#[derive(Clone)]
pub(crate) struct AcceleratorAttribution {
    view: AttributionView,
    telemetry: Option<Arc<WorkloadTelemetry>>,
    memory_port: Option<Arc<dyn ProcessMemoryPort>>,
}

impl AcceleratorAttribution {
    pub(crate) fn new(inventory: Arc<dyn HardwareInventorySource>) -> Self {
        Self {
            view: AttributionView {
                inventory,
                processes: Arc::new(Mutex::new(BTreeMap::new())),
                runtime_memory: Arc::new(Mutex::new(RuntimeMemory::default())),
            },
            telemetry: None,
            memory_port: None,
        }
    }

    /// Sources workload process memory from the runtime that reports it, read-only.
    pub(crate) fn with_process_memory(mut self, port: Arc<dyn ProcessMemoryPort>) -> Self {
        self.memory_port = Some(port);
        self
    }

    /// Records workload resource telemetry for ready deployments through the sink.
    pub(crate) fn with_telemetry(mut self, sink: Option<Arc<dyn TelemetrySink>>) -> Self {
        self.telemetry = sink.map(|sink| Arc::new(WorkloadTelemetry::new(sink)));
        self
    }

    /// Projects the deployment's current usage from the identities this process observed,
    /// against one hardware collection.
    pub(crate) fn projection(
        &self,
        desired: &ModelDeploymentDeclaration,
        state: &ModelDeploymentState,
        target: Option<&ExecutionTarget>,
        collection: &HardwareCollection,
    ) -> AcceleratorMemoryObservation {
        self.view.projection(desired, state, target, collection)
    }

    /// The node's current hardware collection, for a reader that attributes outside a status.
    pub(crate) fn collection(&self) -> HardwareCollection {
        self.view.collection()
    }

    /// Starts resource telemetry for a ready realization unless it is already sampled.
    /// The realization is built only when telemetry is recorded and the generation is new.
    pub(super) fn ready(
        &self,
        deployment_id: &str,
        generation: u64,
        realization: impl FnOnce() -> Option<ReadyRealization>,
    ) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        if telemetry.samples(deployment_id, generation) {
            return;
        }
        if let Some(realization) = realization() {
            telemetry.start(realization, self.view.clone());
        }
    }

    /// Reads the process memory of every tracked workload for the collection reported as
    /// `revision`. A reading of a workload whose tracking ended or changed meanwhile is dropped.
    async fn read_runtime_memory(&self, revision: u64) {
        let Some(port) = &self.memory_port else {
            return;
        };
        let tracked = self
            .view
            .runtime_memory()
            .tracked
            .iter()
            .map(|(deployment_id, tracked)| (deployment_id.clone(), tracked.clone()))
            .collect::<Vec<_>>();
        let readings = futures_util::future::join_all(tracked.into_iter().map(
            |(deployment_id, tracked)| async move {
                let reading = port
                    .observe(
                        &tracked.execution_target_id,
                        &tracked.workload_id,
                        tracked.realization.host_process.as_ref(),
                    )
                    .await;
                (deployment_id, tracked, reading)
            },
        ))
        .await;
        let mut memory = self.view.runtime_memory();
        let readings = readings
            .into_iter()
            .filter(|(deployment_id, tracked, _)| {
                memory.tracked.get(deployment_id) == Some(tracked)
            })
            .map(|(deployment_id, _, reading)| (deployment_id, reading))
            .collect();
        memory.record(revision, readings);
    }

    /// Records the observed workload's process identity, and reads its process memory with every
    /// collection while it runs, when `target`'s runtime reports it. Without a target, a reading
    /// already taken continues but none starts.
    pub(super) fn observe(
        &self,
        deployment_id: &str,
        target: Option<&ExecutionTarget>,
        observation: &WorkloadObservation,
    ) {
        self.track_runtime_memory(deployment_id, target, observation);
        let Some(workload) = observation_identity(observation) else {
            self.forget(deployment_id);
            return;
        };
        let identity = matches!(
            observation,
            WorkloadObservation::Starting(_)
                | WorkloadObservation::Running(_)
                | WorkloadObservation::Stopping(_)
                | WorkloadObservation::Unavailable(_)
        )
        .then(|| workload.host_process.clone())
        .flatten();
        let mut processes = self
            .view
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let workload_id = workload.workload_id.0.clone();
        match identity {
            Some(identity) => {
                let workloads = processes.entry(deployment_id.to_owned()).or_default();
                // A process keeps the revision it was first recorded at, so the collections that
                // may predate it stay bounded however often it is observed again.
                if workloads
                    .get(&workload_id)
                    .is_none_or(|recorded| recorded.identity != identity)
                {
                    let observable_from = self.view.inventory.next_revision();
                    debug!(
                        event_kind = "workload_process_identity_recorded",
                        model_deployment_id = %deployment_id,
                        workload_id = %workload_id,
                        process_id = identity.process_id,
                        observable_from,
                        "Workload process identity recorded for memory attribution"
                    );
                    workloads.insert(
                        workload_id,
                        RecordedProcess {
                            identity,
                            observable_from,
                        },
                    );
                }
            }
            None => {
                if let Some(workloads) = processes.get_mut(deployment_id) {
                    workloads.remove(&workload_id);
                    if workloads.is_empty() {
                        processes.remove(deployment_id);
                    }
                }
            }
        }
    }

    pub(super) fn forget(&self, deployment_id: &str) {
        self.view.runtime_memory().forget(deployment_id);
        self.view
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(deployment_id);
        if let Some(telemetry) = &self.telemetry {
            telemetry.stop(deployment_id);
        }
    }

    /// Keeps reading the running realization already read. Any other observation stops the
    /// reading and drops its readings, so a reading never outlives its workload; a running one
    /// then starts a new reading when the target's runtime reports process memory. A running
    /// observation that momentarily cannot name its engine process keeps reading, and an engine
    /// relaunched in place is read again from its own first running observation.
    fn track_runtime_memory(
        &self,
        deployment_id: &str,
        target: Option<&ExecutionTarget>,
        observation: &WorkloadObservation,
    ) {
        let running = match observation {
            WorkloadObservation::Running(workload) => Some((
                workload,
                TrackedRealization {
                    generation: workload.correlation.reconcile_generation,
                    host_process: workload.host_process.clone(),
                },
            )),
            _ => None,
        };
        let mut memory = self.view.runtime_memory();
        if let (Some(tracked), Some((_, realization))) =
            (memory.tracked.get(deployment_id), &running)
            && tracked.realization.continues_as(realization)
        {
            return;
        }
        memory.forget(deployment_id);
        let (Some((workload, realization)), Some(target)) = (running, target) else {
            return;
        };
        let reported = attribution_scope(
            true,
            target,
            accelerator_presence(&target.accelerator_device_ids),
        )
        .is_some_and(|scope| scope.source == AttributionSource::RuntimeReported);
        if self.memory_port.is_none() || !reported {
            return;
        }
        // Taken under the lock, so every collection from this revision on reads this workload.
        let observable_from = self.view.inventory.next_revision();
        debug!(
            event_kind = "workload_process_memory_tracked",
            model_deployment_id = %deployment_id,
            execution_target_id = %target.execution_target_id,
            generation = realization.generation,
            observable_from,
            "Runtime-reported workload process memory is read with each collection"
        );
        memory.tracked.insert(
            deployment_id.to_owned(),
            TrackedWorkload {
                realization,
                execution_target_id: target.execution_target_id.clone(),
                workload_id: workload.workload_id.clone(),
                observable_from,
            },
        );
    }
}

impl CollectionReading for AcceleratorAttribution {
    fn read(&self, revision: u64) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(self.read_runtime_memory(revision))
    }
}

/// Shared read access to attributed process identities and the hardware inventory. Each
/// projection reads the one collection its caller pinned, so a caller attributing several
/// deployments reads the same collection for all of them.
#[derive(Clone)]
pub(super) struct AttributionView {
    inventory: Arc<dyn HardwareInventorySource>,
    processes: Arc<Mutex<BTreeMap<String, BTreeMap<String, RecordedProcess>>>>,
    runtime_memory: Arc<Mutex<RuntimeMemory>>,
}

impl AttributionView {
    pub(super) fn projection(
        &self,
        desired: &ModelDeploymentDeclaration,
        state: &ModelDeploymentState,
        target: Option<&ExecutionTarget>,
        collection: &HardwareCollection,
    ) -> AcceleratorMemoryObservation {
        let mut identities = BTreeSet::new();
        let mut recorded_after = BTreeSet::new();
        if let Some(workloads) = self
            .processes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&desired.model_deployment_id)
        {
            for recorded in workloads.values() {
                if recorded.observable_from > collection.revision() {
                    recorded_after.insert(recorded.identity.clone());
                }
                identities.insert(recorded.identity.clone());
            }
        }
        let reading = self
            .runtime_memory()
            .reading(collection.revision(), &desired.model_deployment_id);
        project_with_runtime(
            desired,
            state,
            target,
            &identities,
            &recorded_after,
            reading.as_ref(),
            collection,
        )
    }

    pub(super) fn collection(&self) -> HardwareCollection {
        self.inventory.collection()
    }

    fn runtime_memory(&self) -> std::sync::MutexGuard<'_, RuntimeMemory> {
        self.runtime_memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A workload process identity this node recorded, and the revision of the first collection that
/// began reading hardware after it was recorded: earlier collections may predate the process.
#[derive(Clone, Debug)]
struct RecordedProcess {
    identity: HostProcessIdentity,
    observable_from: u64,
}

/// Read-only access to the process memory a runtime reports for its workloads. It carries no
/// lifecycle powers, so attribution can never act on a runtime.
pub(crate) trait ProcessMemoryPort: Send + Sync {
    /// Reads the workload's process memory. With an expected engine process, a figure measured
    /// on any other process, such as an engine relaunched in place, reads as not running.
    fn observe<'a>(
        &'a self,
        execution_target_id: &'a str,
        workload_id: &'a WorkloadId,
        expected_process: Option<&'a HostProcessIdentity>,
    ) -> Pin<Box<dyn Future<Output = RuntimeMemoryReading> + Send + 'a>>;
}

impl ProcessMemoryPort for RuntimeTargetMap {
    fn observe<'a>(
        &'a self,
        execution_target_id: &'a str,
        workload_id: &'a WorkloadId,
        expected_process: Option<&'a HostProcessIdentity>,
    ) -> Pin<Box<dyn Future<Output = RuntimeMemoryReading> + Send + 'a>> {
        Box::pin(async move {
            match self
                .observe_process_memory(execution_target_id, workload_id)
                .await
            {
                Ok(observation) => runtime_reading(observation, expected_process),
                Err(error) => {
                    debug!(
                        event_kind = "workload_process_memory_observation_failed",
                        execution_target_id = %execution_target_id,
                        error = %error,
                        "Runtime workload process memory could not be observed"
                    );
                    RuntimeMemoryReading::Unavailable {
                        code: "WORKLOAD_PROCESS_MEMORY_UNAVAILABLE",
                        message: "The runtime could not observe workload process memory."
                            .to_owned(),
                    }
                }
            }
        })
    }
}

/// One runtime-reported process memory figure, or why it has none.
#[derive(Clone, Debug)]
pub(crate) enum RuntimeMemoryReading {
    Available { bytes: u64, observed_at: SystemTime },
    Unavailable { code: &'static str, message: String },
    NotReported,
}

/// Collections whose runtime-reported readings are kept: the latest published one, which a status
/// may have pinned, the one being collected, and one more for a status still building.
const READ_COLLECTIONS_KEPT: usize = 3;

/// Runtime-reported process memory: the workloads it is read for and the readings taken with
/// each recent collection, so a projection takes only the readings of the collection it pinned.
#[derive(Default)]
struct RuntimeMemory {
    tracked: BTreeMap<String, TrackedWorkload>,

    /// Readings by deployment, per collection revision, oldest first.
    readings: VecDeque<(u64, BTreeMap<String, RuntimeMemoryReading>)>,
}

impl RuntimeMemory {
    /// The deployment's reading taken with the collection reported as `revision`. A workload
    /// tracked before that collection began misses its reading only when it timed out or was
    /// evicted, so it reads unavailable; None means no reading of it was due yet.
    fn reading(&self, revision: u64, deployment_id: &str) -> Option<RuntimeMemoryReading> {
        let reading = self
            .readings
            .iter()
            .find(|(read, _)| *read == revision)
            .and_then(|(_, readings)| readings.get(deployment_id));
        if let Some(reading) = reading {
            return Some(reading.clone());
        }
        self.tracked
            .get(deployment_id)
            .filter(|tracked| tracked.observable_from <= revision)
            .map(|_| RuntimeMemoryReading::Unavailable {
                code: "WORKLOAD_PROCESS_MEMORY_UNAVAILABLE",
                message: "The runtime reported no workload process memory with this collection."
                    .to_owned(),
            })
    }

    /// Keeps the readings taken with the collection reported as `revision`.
    fn record(&mut self, revision: u64, readings: BTreeMap<String, RuntimeMemoryReading>) {
        self.readings.push_back((revision, readings));
        while self.readings.len() > READ_COLLECTIONS_KEPT {
            self.readings.pop_front();
        }
    }

    /// Stops reading the deployment and drops every reading of it.
    fn forget(&mut self, deployment_id: &str) {
        self.tracked.remove(deployment_id);
        for (_, readings) in &mut self.readings {
            readings.remove(deployment_id);
        }
    }
}

/// A workload whose process memory its runtime reports, read with each collection from
/// `observable_from` on: earlier collections may have begun before it was tracked.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedWorkload {
    realization: TrackedRealization,
    execution_target_id: String,
    workload_id: WorkloadId,
    observable_from: u64,
}

/// The running realization a reading measures: its generation and the engine process observed.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TrackedRealization {
    generation: u64,
    host_process: Option<HostProcessIdentity>,
}

impl TrackedRealization {
    /// Reports whether an observed realization is still this one. An observation without an
    /// engine process identity, such as a transient failure to read the runtime's process
    /// record, is unknown rather than a different engine.
    fn continues_as(&self, observed: &Self) -> bool {
        self.generation == observed.generation
            && (observed.host_process.is_none() || observed.host_process == self.host_process)
    }
}

/// Owner projection of a node-hosted deployment's memory: the observation its hosting node
/// reported with current status, else a stable unavailable reason without a refresh state, so a
/// deployment whose node status is missing or stale never reads as a pending refresh.
pub(super) fn node_hosted_projection(
    desired: &ModelDeploymentDeclaration,
    state: &ModelDeploymentState,
    target: Option<&ExecutionTarget>,
    reported: Option<AcceleratorMemoryObservation>,
) -> AcceleratorMemoryObservation {
    if let Some(reported) = reported {
        return reported;
    }
    match deployment_scope(desired, state, target) {
        Some(_) => node_status_unavailable(),
        None => not_applicable(),
    }
}

/// Projects from the runtime-reported memory when the target's runtime attributes it, falling back
/// to process-identity attribution when the runtime reports none. `recorded_after` names the
/// identities recorded after `collection` began reading hardware.
fn project_with_runtime(
    desired: &ModelDeploymentDeclaration,
    state: &ModelDeploymentState,
    target: Option<&ExecutionTarget>,
    identities: &BTreeSet<HostProcessIdentity>,
    recorded_after: &BTreeSet<HostProcessIdentity>,
    reading: Option<&RuntimeMemoryReading>,
    collection: &HardwareCollection,
) -> AcceleratorMemoryObservation {
    let Some(scope) = deployment_scope(desired, state, target) else {
        return not_applicable();
    };
    if scope.source == AttributionSource::RuntimeReported {
        match reading {
            Some(RuntimeMemoryReading::Available { bytes, observed_at }) => {
                let managed_node_id = &scope.node;
                // The row names a hardware device id, so a selector that does not resolve to
                // exactly one observed device leaves the row unattributed rather than guessed.
                let device_id = target
                    .map(|target| {
                        ResolvedAccelerators::resolve(
                            &target.accelerator_device_ids,
                            collection
                                .entry(managed_node_id)
                                .and_then(|entry| entry.snapshot.as_ref()),
                        )
                    })
                    .and_then(|resolved| resolved.single_id().map(str::to_owned))
                    .unwrap_or_default();
                return AcceleratorMemoryObservation {
                    observed_at: Some(timestamp(*observed_at)),
                    refresh: None,
                    result: Some(accelerator_memory_observation::Result::Available(
                        AcceleratorMemoryUsageSet {
                            devices: vec![AcceleratorMemoryDeviceUsage {
                                managed_node_id: managed_node_id.as_str().to_owned(),
                                device_id,
                                used_memory_bytes: *bytes,
                            }],
                        },
                    )),
                };
            }
            Some(RuntimeMemoryReading::Unavailable { code, message }) => {
                return unavailable(code, message);
            }
            Some(RuntimeMemoryReading::NotReported) => {}
            None => {
                return unavailable(
                    PROCESS_MEMORY_PENDING,
                    "The runtime has not yet reported workload process memory.",
                );
            }
        }
    }
    project(
        desired,
        state,
        target,
        identities,
        recorded_after,
        collection,
    )
}

fn project(
    desired: &ModelDeploymentDeclaration,
    state: &ModelDeploymentState,
    target: Option<&ExecutionTarget>,
    identities: &BTreeSet<HostProcessIdentity>,
    recorded_after: &BTreeSet<HostProcessIdentity>,
    collection: &HardwareCollection,
) -> AcceleratorMemoryObservation {
    match deployment_scope(desired, state, target) {
        Some(scope) => project_identities(&scope, identities, recorded_after, collection),
        None => not_applicable(),
    }
}

/// Returns where memory attribution applies to the deployment by the shared scope rule, or None
/// when it does not: no target, an upstream deployment, or one that is paused, unloading, or
/// failed, besides the targets the shared rule excludes.
fn deployment_scope(
    desired: &ModelDeploymentDeclaration,
    state: &ModelDeploymentState,
    target: Option<&ExecutionTarget>,
) -> Option<AttributionScope> {
    let upstream = desired.snapshot.as_ref().is_some_and(|snapshot| {
        matches!(
            snapshot.source,
            Some(model_deployment_snapshot::Source::Upstream(_))
        )
    });
    let inactive = matches!(
        ModelDeploymentStatus::try_from(state.status).ok(),
        Some(
            ModelDeploymentStatus::Paused
                | ModelDeploymentStatus::Unloading
                | ModelDeploymentStatus::Failed
        )
    );
    let target = target?;
    attribution_scope(
        !upstream && !inactive,
        target,
        accelerator_presence(&target.accelerator_device_ids),
    )
}

/// Converts a runtime process memory observation into a reading. A figure measured on another
/// engine process than the expected one, such as one relaunched in place before it is observed,
/// reads as not running, so a reading always describes the process being tracked.
fn runtime_reading(
    observation: ProcessMemoryObservation,
    expected_process: Option<&HostProcessIdentity>,
) -> RuntimeMemoryReading {
    let not_running = || RuntimeMemoryReading::Unavailable {
        code: "WORKLOAD_PROCESS_NOT_RUNNING",
        message: "The workload process is not running.".to_owned(),
    };
    match observation.memory {
        ProcessMemory::Available { .. }
            if expected_process
                .is_some_and(|expected| observation.process.as_ref() != Some(expected)) =>
        {
            not_running()
        }
        ProcessMemory::Available { bytes } => RuntimeMemoryReading::Available {
            bytes,
            observed_at: observation.observed_at,
        },
        ProcessMemory::Unavailable(ProcessMemoryUnavailable::NotRunning) => not_running(),
        ProcessMemory::Unavailable(ProcessMemoryUnavailable::Read(_)) => {
            RuntimeMemoryReading::Unavailable {
                code: "WORKLOAD_PROCESS_MEMORY_UNAVAILABLE",
                message: "The runtime could not read a complete workload process memory figure."
                    .to_owned(),
            }
        }
        ProcessMemory::NotReported => RuntimeMemoryReading::NotReported,
    }
}

fn observation_identity(observation: &WorkloadObservation) -> Option<&ObservedWorkload> {
    match observation {
        WorkloadObservation::Starting(value)
        | WorkloadObservation::Running(value)
        | WorkloadObservation::Stopping(value)
        | WorkloadObservation::Stopped {
            workload: value, ..
        }
        | WorkloadObservation::Unavailable(value)
        | WorkloadObservation::Absent(value) => Some(value),
        WorkloadObservation::Unknown { .. } => None,
    }
}

fn timestamp(time: SystemTime) -> Timestamp {
    let elapsed = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
        nanos: elapsed.subsec_nanos() as i32,
    }
}
#[cfg(test)]
pub(super) mod tests {
    use std::time::Duration;

    use dotcore_domain::hardware_observation::{
        AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
        AcceleratorVendorProcessResult, NodeHardwareObservation, ObservedAcceleratorProcess,
    };
    use dotcore_host_process::ProcessLifetime;
    use dotcore_proto::{
        dotcore::{
            deployment::v1::AcceleratorMemoryUnavailable,
            model::v1::{ModelDeploymentSnapshot, model_deployment_snapshot},
            observation::v1::{
                AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, AcceleratorVendor,
                HardwareSnapshot, accelerator_observation,
            },
        },
        google::protobuf::Timestamp,
    };
    use dotcore_runtime::{RuntimeBackendId, WorkloadCorrelation, WorkloadId};
    use dotcore_site::node::hardware_inventory::{
        HardwareInventoryEntry, HardwareInventoryPublisher,
    };

    use dotcore_domain::SiteNodeId;
    use dotcore_proto::dotcore::site::v1::{execution_target, model_runtime_binding};

    use super::*;
    use crate::hardware::attribution::pending_unavailable as unavailable_pending;

    #[test]
    fn exact_lifetime_identity_projects_sorted_usage_and_excludes_external_processes() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(vec![
                        process("GPU-b", 41, 700, Some(20)),
                        process("GPU-a", 41, 700, Some(10)),
                        process("GPU-a", 99, 900, Some(1_000)),
                    ]),
                },
                timestamp(11),
            ),
        );
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);

        let observation = project(
            &desired(),
            &state(),
            Some(&target()),
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(observation.observed_at, Some(timestamp(10)));
        assert_eq!(
            observation.result,
            Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![
                        AcceleratorMemoryDeviceUsage {
                            managed_node_id: SiteNodeId::for_test("node-a").to_string(),
                            device_id: "GPU-a".to_owned(),
                            used_memory_bytes: 10,
                        },
                        AcceleratorMemoryDeviceUsage {
                            managed_node_id: SiteNodeId::for_test("node-a").to_string(),
                            device_id: "GPU-b".to_owned(),
                            used_memory_bytes: 20,
                        },
                    ],
                },
            ))
        );
    }

    #[test]
    fn child_processes_attribute_their_memory_to_the_workload_root() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(
            &publisher,
            vec![
                child_process("GPU-a", 1275, 800, &[(41, 700), (1, 1)], Some(24_000)),
                child_process("GPU-a", 1276, 850, &[(1275, 800), (41, 700)], Some(1_000)),
                child_process("GPU-a", 99, 900, &[(1, 1)], Some(5_000)),
            ],
        );
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);

        let observation = project(
            &desired(),
            &state(),
            Some(&target()),
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("GPU-a", 25_000)]));
    }

    #[test]
    fn an_ancestor_with_a_reused_pid_does_not_attribute_the_child() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(
            &publisher,
            vec![child_process(
                "GPU-a",
                1275,
                800,
                &[(41, 650)],
                Some(24_000),
            )],
        );
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);

        let observation = project(
            &desired(),
            &state(),
            Some(&target()),
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(
            observation.result,
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "PROCESS_NOT_OBSERVED".to_owned(),
                    message:
                        "An authoritative workload process was not observed on an accelerator."
                            .to_owned(),
                }
            ))
        );
    }

    #[test]
    fn a_row_matching_two_workload_roots_counts_once() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(
            &publisher,
            vec![
                child_process("GPU-a", 1275, 800, &[(41, 700), (1, 1)], Some(24_000)),
                process("GPU-a", 41, 700, Some(100)),
            ],
        );
        let identities =
            BTreeSet::from([identity("node-a", 41, 700), identity("node-a", 1275, 800)]);

        let observation = project(
            &desired(),
            &state(),
            Some(&target()),
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("GPU-a", 24_100)]));
    }

    #[test]
    fn different_nodes_keep_the_same_process_lifetime_distinct() {
        let first = identity("node-a", 41, 700);
        let second = identity("node-b", 41, 700);
        let mut processes = BTreeMap::new();
        processes.insert(first.clone(), "first");
        processes.insert(second.clone(), "second");

        assert_ne!(first, second);
        assert_eq!(processes.keys().collect::<Vec<_>>(), vec![&first, &second]);
        assert_eq!(processes.get(&identity("node-a", 41, 700)), Some(&"first"));
        assert_eq!(processes.get(&identity("node-b", 41, 700)), Some(&"second"));
    }

    #[test]
    fn a_workload_process_on_another_node_cannot_be_attributed() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.publish(
            &SiteNodeId::for_test("node-b"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(vec![process(
                        "GPU-a",
                        41,
                        700,
                        Some(10),
                    )]),
                },
                timestamp(11),
            ),
        );
        let attribution = AcceleratorAttribution::new(Arc::new(inventory));
        attribution.observe(
            "deployment-a",
            None,
            &WorkloadObservation::Running(workload("node-b", "workload-a", 41, 700)),
        );

        let observation = attribution.projection(
            &desired(),
            &state(),
            Some(&target()),
            &attribution.collection(),
        );

        assert_eq!(
            observation,
            unavailable_pending(
                "WORKLOAD_NODE_MISMATCH",
                "Runtime reported a workload process outside the deployment execution target.",
            )
        );
    }

    #[test]
    fn unavailable_vendor_process_evidence_cannot_be_hidden_by_matching_rows() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: AcceleratorProcessObservation::Observed(vec![
                        AcceleratorVendorProcessObservation {
                            vendor: AcceleratorVendor::Nvidia,
                            result: AcceleratorVendorProcessResult::Observed(vec![process(
                                "GPU-a",
                                41,
                                700,
                                Some(10),
                            )]),
                        },
                        AcceleratorVendorProcessObservation {
                            vendor: AcceleratorVendor::Amd,
                            result: AcceleratorVendorProcessResult::Unavailable(
                                dotcore_proto::dotcore::observation::v1::ObservationUnavailable {
                                    code: "COLLECTION_FAILED".to_owned(),
                                    message: "AMD process observation failed.".to_owned(),
                                },
                            ),
                        },
                    ]),
                },
                timestamp(11),
            ),
        );
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);

        let observation = project(
            &desired(),
            &state(),
            Some(&target()),
            &identities,
            &BTreeSet::new(),
            &inventory.collection(),
        );

        assert_eq!(
            observation.result,
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "COLLECTION_FAILED".to_owned(),
                    message: "AMD process observation failed.".to_owned(),
                }
            ))
        );
    }

    #[test]
    fn apple_nodes_never_attribute_deployment_memory_from_node_process_rows() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let attribution = AcceleratorAttribution::new(Arc::new(inventory));
        attribution.observe(
            "deployment-a",
            None,
            &WorkloadObservation::Running(workload("node-a", "workload-a", 41, 700)),
        );
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: AcceleratorProcessObservation::Observed(vec![
                        AcceleratorVendorProcessObservation {
                            vendor: AcceleratorVendor::Apple,
                            result: AcceleratorVendorProcessResult::Observed(Vec::new()),
                        },
                    ]),
                },
                timestamp(11),
            ),
        );
        let simulated_target = ExecutionTarget {
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..target()
        };

        let docker = attribution.projection(
            &desired(),
            &state(),
            Some(&target()),
            &attribution.collection(),
        );
        let simulated = attribution.projection(
            &desired(),
            &state(),
            Some(&simulated_target),
            &attribution.collection(),
        );

        // An identified workload, Docker or simulated, is never given a false zero from the empty
        // Apple rows.
        assert_eq!(simulated, docker);
        assert_eq!(
            docker.result,
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "PROCESS_NOT_OBSERVED".to_owned(),
                    message:
                        "An authoritative workload process was not observed on an accelerator."
                            .to_owned(),
                }
            ))
        );
    }

    #[test]
    fn distinct_workloads_accumulate_on_one_deployment_device() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(vec![
                        process("GPU-a", 41, 700, Some(10)),
                        process("GPU-a", 42, 701, Some(20)),
                    ]),
                },
                timestamp(11),
            ),
        );
        let attribution = AcceleratorAttribution::new(Arc::new(inventory));
        attribution.observe(
            "deployment-a",
            None,
            &WorkloadObservation::Running(workload("node-a", "workload-a", 41, 700)),
        );
        attribution.observe(
            "deployment-a",
            None,
            &WorkloadObservation::Running(workload("node-a", "workload-b", 42, 701)),
        );

        let observation = attribution.projection(
            &desired(),
            &state(),
            Some(&target()),
            &attribution.collection(),
        );

        let Some(accelerator_memory_observation::Result::Available(usage)) = observation.result
        else {
            panic!("two authoritative workloads must produce available usage");
        };
        assert_eq!(usage.devices[0].used_memory_bytes, 30);
    }

    #[test]
    fn a_process_recorded_after_the_pinned_collection_reads_pending_until_the_next_collection() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(&publisher, Vec::new());
        let attribution = AcceleratorAttribution::new(Arc::new(inventory));
        let pinned = attribution.collection();
        let running = WorkloadObservation::Running(workload("node-a", "workload-a", 41, 700));
        attribution.observe("deployment-a", None, &running);
        let project = |collection: &HardwareCollection| {
            attribution
                .projection(&desired(), &state(), Some(&target()), collection)
                .result
        };
        let pending = Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: "WORKLOAD_PROCESS_MEMORY_PENDING".to_owned(),
                message: "Workload process memory has not been observed since the process started."
                    .to_owned(),
            },
        ));

        // The pinned collection was read before the process was recorded.
        assert_eq!(project(&pinned), pending);

        // The next collection measures the process when it shows it.
        publish_node_a(&publisher, vec![process("GPU-a", 41, 700, Some(10))]);
        assert_eq!(
            project(&attribution.collection()),
            node_a_usage(&[("GPU-a", 10)])
        );

        // A collection that doesn't show it reads it not observed: observing the same process
        // again never moves when it was recorded, so the pending reading is bounded.
        publish_node_a(&publisher, Vec::new());
        attribution.observe("deployment-a", None, &running);
        assert_eq!(
            project(&attribution.collection()),
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "PROCESS_NOT_OBSERVED".to_owned(),
                    message:
                        "An authoritative workload process was not observed on an accelerator."
                            .to_owned(),
                }
            ))
        );
        assert_eq!(project(&pinned), pending);

        // A restarted process is a new identity, recorded anew.
        let restarted = WorkloadObservation::Running(workload("node-a", "workload-a", 41, 900));
        attribution.observe("deployment-a", None, &restarted);
        assert_eq!(project(&attribution.collection()), pending);
    }

    #[test]
    fn node_hosted_projection_uses_the_reported_observation_else_a_stable_unavailable_code() {
        let reported = AcceleratorMemoryObservation {
            observed_at: Some(timestamp(10)),
            refresh: None,
            result: node_a_usage(&[("GPU-a", 10)]),
        };
        assert_eq!(
            node_hosted_projection(
                &desired(),
                &state(),
                Some(&target()),
                Some(reported.clone())
            ),
            reported
        );

        let expected = AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "NODE_STATUS_UNAVAILABLE".to_owned(),
                    message:
                        "The hosting node has not reported current status for this deployment."
                            .to_owned(),
                },
            )),
        };
        for status in [
            ModelDeploymentStatus::Ready,
            ModelDeploymentStatus::Loading,
            ModelDeploymentStatus::Unavailable,
        ] {
            let state = ModelDeploymentState {
                status: status as i32,
                ..Default::default()
            };
            assert_eq!(
                node_hosted_projection(&desired(), &state, Some(&target()), None),
                expected,
                "{status:?}"
            );
        }
        let paused = ModelDeploymentState {
            status: ModelDeploymentStatus::Paused as i32,
            ..Default::default()
        };
        assert_eq!(
            node_hosted_projection(&desired(), &paused, Some(&target()), None),
            not_applicable()
        );
    }

    #[test]
    fn simulated_target_attributes_only_simulated_processes() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(&publisher, vec![process("GPU-a", 41, 700, Some(10))]);
        let simulated = simulated_target();

        assert_eq!(
            project(
                &desired(),
                &state(),
                Some(&simulated),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &inventory.collection()
            ),
            not_applicable()
        );
        assert_eq!(
            project(
                &desired(),
                &state(),
                Some(&simulated),
                &BTreeSet::from([identity("node-a", 41, 700)]),
                &BTreeSet::new(),
                &inventory.collection(),
            )
            .result,
            node_a_usage(&[("GPU-a", 10)])
        );
    }

    /// Projects one Ready deployment of `target()` with the given identities.
    pub(in crate::model::controller) fn project_for_test(
        collection: &HardwareCollection,
        identities: &BTreeSet<HostProcessIdentity>,
    ) -> AcceleratorMemoryObservation {
        project(
            &desired(),
            &state(),
            Some(&target()),
            identities,
            &BTreeSet::new(),
            collection,
        )
    }

    #[test]
    fn native_runtime_memory_projects_without_device_ids_or_inventory() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let native = native_target(Vec::new());
        let reading = RuntimeMemoryReading::Available {
            bytes: 4_096,
            observed_at: UNIX_EPOCH + Duration::from_secs(30),
        };

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&native),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(&reading),
            &inventory.collection(),
        );

        assert_eq!(observation.observed_at, Some(timestamp(30)));
        assert_eq!(observation.result, node_a_usage(&[("", 4_096)]));
    }

    #[test]
    fn native_runtime_memory_uses_the_resolved_target_device_id() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        accelerators: vec![AcceleratorObservation {
                            vendor: AcceleratorVendor::Apple as i32,
                            result: Some(accelerator_observation::Result::Facts(
                                AcceleratorFactsSet {
                                    facts: vec![AcceleratorFacts {
                                        vendor: AcceleratorVendor::Apple as i32,
                                        device_id: "apple-gpu-0".to_owned(),
                                        vendor_index: Some(0),
                                        ..Default::default()
                                    }],
                                },
                            )),
                        }],
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(Vec::new()),
                },
                timestamp(11),
            ),
        );
        let native = native_target(vec!["apple-gpu-0".to_owned()]);
        let reading = RuntimeMemoryReading::Available {
            bytes: 7,
            observed_at: UNIX_EPOCH + Duration::from_secs(30),
        };

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&native),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(&reading),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("apple-gpu-0", 7)]));
    }

    #[test]
    fn native_runtime_memory_of_a_multi_device_target_names_no_device() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let gpu = |device_id: &str, index: u32| AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            device_id: device_id.to_owned(),
            vendor_index: Some(index),
            ..Default::default()
        };
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        accelerators: vec![AcceleratorObservation {
                            vendor: AcceleratorVendor::Nvidia as i32,
                            result: Some(accelerator_observation::Result::Facts(
                                AcceleratorFactsSet {
                                    facts: vec![gpu("GPU-first", 0), gpu("GPU-second", 1)],
                                },
                            )),
                        }],
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(Vec::new()),
                },
                timestamp(11),
            ),
        );
        let native = native_target(vec![
            "nvidia.com/gpu=0".to_owned(),
            "nvidia.com/gpu=1".to_owned(),
        ]);
        let reading = RuntimeMemoryReading::Available {
            bytes: 7,
            observed_at: UNIX_EPOCH + Duration::from_secs(30),
        };

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&native),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(&reading),
            &inventory.collection(),
        );

        // Two resolved devices cannot name one usage row, so it stays unattributed.
        assert_eq!(observation.result, node_a_usage(&[("", 7)]));
    }

    #[test]
    fn native_runtime_memory_without_a_snapshot_names_no_device() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let native = native_target(vec!["apple-gpu-0".to_owned()]);
        let reading = RuntimeMemoryReading::Available {
            bytes: 7,
            observed_at: UNIX_EPOCH + Duration::from_secs(30),
        };

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&native),
            &BTreeSet::new(),
            &BTreeSet::new(),
            Some(&reading),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("", 7)]));
    }

    #[test]
    fn native_runtime_memory_unavailability_is_explicit() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let native = native_target(Vec::new());
        let unavailable = |reading: Option<&RuntimeMemoryReading>| {
            project_with_runtime(
                &desired(),
                &state(),
                Some(&native),
                &BTreeSet::new(),
                &BTreeSet::new(),
                reading,
                &inventory.collection(),
            )
            .result
        };
        let expected = |code: &str, message: &str| {
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: code.to_owned(),
                    message: message.to_owned(),
                },
            ))
        };

        assert_eq!(
            unavailable(None),
            expected(
                "WORKLOAD_PROCESS_MEMORY_PENDING",
                "The runtime has not yet reported workload process memory."
            )
        );
        assert_eq!(
            unavailable(Some(&RuntimeMemoryReading::Unavailable {
                code: "WORKLOAD_PROCESS_NOT_RUNNING",
                message: "The workload process is not running.".to_owned(),
            })),
            expected(
                "WORKLOAD_PROCESS_NOT_RUNNING",
                "The workload process is not running."
            )
        );
    }

    #[test]
    fn a_runtime_that_reports_no_memory_falls_back_to_process_identity() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(&publisher, vec![process("GPU-a", 41, 700, Some(10))]);
        let identities = BTreeSet::from([identity("node-a", 41, 700)]);

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&native_target(Vec::new())),
            &identities,
            &BTreeSet::new(),
            Some(&RuntimeMemoryReading::NotReported),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("GPU-a", 10)]));
    }

    #[test]
    fn docker_attribution_ignores_runtime_reported_memory() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish_node_a(&publisher, vec![process("GPU-a", 41, 700, Some(10))]);
        let reading = RuntimeMemoryReading::Available {
            bytes: 999,
            observed_at: UNIX_EPOCH + Duration::from_secs(30),
        };

        let observation = project_with_runtime(
            &desired(),
            &state(),
            Some(&target()),
            &BTreeSet::from([identity("node-a", 41, 700)]),
            &BTreeSet::new(),
            Some(&reading),
            &inventory.collection(),
        );

        assert_eq!(observation.result, node_a_usage(&[("GPU-a", 10)]));
    }

    #[tokio::test]
    async fn a_loading_workload_attributes_the_runtime_reading_taken_with_its_pinned_collection() {
        let fixture = RuntimeFixture::new(3 * GIB);
        let loading = ModelDeploymentState {
            status: ModelDeploymentStatus::Loading as i32,
            ..Default::default()
        };
        fixture.track(running(41, 700));

        let pinned = fixture.read_and_publish().await;
        // The next collection reads more while it loads, before it is published.
        fixture.port.set_bytes(5 * GIB);
        let next_revision = fixture.inventory.next_revision();
        fixture.attribution.read(next_revision).await;

        assert_eq!(
            fixture.projection_with(&loading, &pinned),
            node_a_usage(&[("", 3 * GIB)]),
            "the pinned collection takes only its own reading"
        );
        publish_node_a(&fixture.publisher, Vec::new());
        let next = fixture.inventory.collection();
        assert_eq!(next.revision(), next_revision);
        assert_eq!(
            fixture.projection_with(&loading, &next),
            node_a_usage(&[("", 5 * GIB)])
        );
        assert_eq!(
            fixture.port.calls(),
            vec![
                (
                    "target-a".to_owned(),
                    "workload-a".to_owned(),
                    Some(identity("node-a", 41, 700))
                );
                2
            ]
        );
    }

    #[tokio::test]
    async fn a_ready_workload_missing_its_reading_reads_unavailable_rather_than_pending() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        let read = fixture.read_and_publish().await;
        assert_eq!(fixture.projection(&read), node_a_usage(&[("", GIB)]));

        // A collection published without a reading of the workload it already tracked, as when
        // the reading ran out of time: neither pending nor another collection's reading.
        publish_node_a(&fixture.publisher, Vec::new());
        let unread = fixture.inventory.collection();

        assert_eq!(unread.revision(), read.revision() + 1);
        assert_eq!(
            fixture.projection(&unread),
            Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "WORKLOAD_PROCESS_MEMORY_UNAVAILABLE".to_owned(),
                    message:
                        "The runtime reported no workload process memory with this collection."
                            .to_owned(),
                }
            ))
        );
        // Before the first collection, no reading was due yet.
        assert_eq!(
            fixture.projection(&HardwareCollection::default()),
            pending_memory()
        );
    }

    #[tokio::test]
    async fn a_workload_tracked_after_its_collection_was_read_reads_pending_until_the_next() {
        let fixture = RuntimeFixture::new(GIB);
        let read_before = fixture.read_and_publish().await;
        fixture.track(running(41, 700));

        assert_eq!(fixture.projection(&read_before), pending_memory());
        let read_after = fixture.read_and_publish().await;
        assert_eq!(fixture.projection(&read_after), node_a_usage(&[("", GIB)]));
    }

    #[tokio::test]
    async fn only_a_running_workload_on_a_runtime_reported_target_is_read() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.attribution.observe(
            "deployment-a",
            Some(&native_target(Vec::new())),
            &WorkloadObservation::Starting(workload("node-a", "workload-a", 41, 700)),
        );
        fixture.attribution.observe(
            "deployment-a",
            Some(&target()),
            &WorkloadObservation::Running(workload("node-a", "workload-a", 41, 700)),
        );

        let collection = fixture.read_and_publish().await;

        assert_eq!(fixture.port.calls(), Vec::new());
        assert_eq!(fixture.projection(&collection), pending_memory());
    }

    #[tokio::test]
    async fn forget_drops_the_readings_of_every_kept_collection() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        let collection = fixture.read_and_publish().await;

        fixture.attribution.forget("deployment-a");

        assert_eq!(fixture.projection(&collection), pending_memory());
        fixture.read_and_publish().await;
        assert_eq!(
            fixture.port.calls().len(),
            1,
            "a forgotten workload is not read"
        );
    }

    #[tokio::test]
    async fn regeneration_shows_pending_instead_of_the_previous_generation_reading() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        let collection = fixture.read_and_publish().await;
        let mut next = workload("node-a", "workload-a", 41, 700);
        next.correlation.reconcile_generation = 2;

        fixture
            .attribution
            .observe("deployment-a", None, &WorkloadObservation::Starting(next));

        assert_eq!(fixture.projection(&collection), pending_memory());
    }

    #[tokio::test]
    async fn leaving_running_stops_reading_runtime_memory() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        fixture.read_and_publish().await;

        fixture.attribution.observe(
            "deployment-a",
            None,
            &WorkloadObservation::Unavailable(workload("node-a", "workload-a", 41, 700)),
        );
        let collection = fixture.read_and_publish().await;

        assert_eq!(fixture.port.calls().len(), 1);
        assert_eq!(fixture.projection(&collection), pending_memory());
    }

    #[tokio::test]
    async fn an_engine_relaunched_in_place_is_read_from_its_first_running_observation() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        let first = fixture.read_and_publish().await;

        // Same generation, new engine process that is still loading.
        let relaunched = running(42, 800);
        fixture
            .attribution
            .observe("deployment-a", None, &relaunched);
        assert_eq!(fixture.projection(&first), pending_memory());
        fixture.track(relaunched);
        fixture.port.set_bytes(2 * GIB);
        let second = fixture.read_and_publish().await;

        assert_eq!(fixture.projection(&second), node_a_usage(&[("", 2 * GIB)]));
        assert_eq!(
            fixture
                .port
                .calls()
                .into_iter()
                .map(|(_, _, process)| process)
                .collect::<Vec<_>>(),
            vec![
                Some(identity("node-a", 41, 700)),
                Some(identity("node-a", 42, 800))
            ]
        );
    }

    #[tokio::test]
    async fn an_unknown_engine_identity_keeps_reading_the_running_engine() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        fixture.read_and_publish().await;
        let mut unknown = workload("node-a", "workload-a", 41, 700);
        unknown.host_process = None;

        fixture.track(WorkloadObservation::Running(unknown));
        let collection = fixture.read_and_publish().await;

        assert_eq!(fixture.projection(&collection), node_a_usage(&[("", GIB)]));
        assert_eq!(
            fixture
                .port
                .calls()
                .last()
                .map(|(_, _, process)| process.clone()),
            Some(Some(identity("node-a", 41, 700)))
        );
    }

    #[tokio::test]
    async fn a_reading_of_a_workload_forgotten_while_it_was_read_is_dropped() {
        let fixture = RuntimeFixture::new(GIB);
        fixture.track(running(41, 700));
        let gate = fixture.port.hold();
        let revision = fixture.inventory.next_revision();
        let reading = tokio::spawn({
            let attribution = fixture.attribution.clone();
            async move { attribution.read(revision).await }
        });
        gate.reached.notified().await;

        fixture.attribution.forget("deployment-a");
        gate.release.notify_one();
        reading.await.expect("reading task");
        publish_node_a(&fixture.publisher, Vec::new());

        assert_eq!(
            fixture.projection(&fixture.inventory.collection()),
            pending_memory()
        );
    }

    #[test]
    fn a_reading_from_another_engine_process_reads_as_not_running() {
        let expected = identity("node-a", 41, 700);
        let reading = |process: Option<HostProcessIdentity>,
                       expected: Option<&HostProcessIdentity>| {
            match runtime_reading(
                ProcessMemoryObservation {
                    observed_at: UNIX_EPOCH + Duration::from_secs(30),
                    memory: ProcessMemory::Available { bytes: 512 },
                    process,
                },
                expected,
            ) {
                RuntimeMemoryReading::Available { bytes, .. } => Ok(bytes),
                RuntimeMemoryReading::Unavailable { code, .. } => Err(code),
                RuntimeMemoryReading::NotReported => Err("NOT_REPORTED"),
            }
        };

        assert_eq!(reading(Some(expected.clone()), Some(&expected)), Ok(512));
        assert_eq!(
            reading(Some(identity("node-a", 42, 800)), Some(&expected)),
            Err("WORKLOAD_PROCESS_NOT_RUNNING")
        );
        assert_eq!(
            reading(None, Some(&expected)),
            Err("WORKLOAD_PROCESS_NOT_RUNNING")
        );
        assert_eq!(reading(Some(identity("node-a", 42, 800)), None), Ok(512));
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    /// A native deployment whose runtime reports its process memory through a settable port,
    /// beside an inventory the test publishes collections to.
    struct RuntimeFixture {
        attribution: AcceleratorAttribution,
        port: Arc<SettablePort>,
        publisher: HardwareInventoryPublisher,
        inventory: dotcore_site::node::hardware_inventory::HardwareInventory,
    }

    impl RuntimeFixture {
        fn new(bytes: u64) -> Self {
            let (publisher, inventory) = HardwareInventoryPublisher::new();
            let port = Arc::new(SettablePort::default());
            port.set_bytes(bytes);
            let attribution = AcceleratorAttribution::new(Arc::new(inventory.clone()))
                .with_process_memory(Arc::clone(&port) as Arc<dyn ProcessMemoryPort>);
            Self {
                attribution,
                port,
                publisher,
                inventory,
            }
        }

        /// Observes the workload on the native target, as the realizer does.
        fn track(&self, observation: WorkloadObservation) {
            self.attribution.observe(
                "deployment-a",
                Some(&native_target(Vec::new())),
                &observation,
            );
        }

        /// Reads runtime memory for the next collection, then publishes it, as a node collects.
        async fn read_and_publish(&self) -> HardwareCollection {
            let revision = self.inventory.next_revision();
            self.attribution.read(revision).await;
            publish_node_a(&self.publisher, Vec::new());
            let collection = self.inventory.collection();
            assert_eq!(collection.revision(), revision);
            collection
        }

        fn projection(
            &self,
            collection: &HardwareCollection,
        ) -> Option<accelerator_memory_observation::Result> {
            self.projection_with(&state(), collection)
        }

        fn projection_with(
            &self,
            state: &ModelDeploymentState,
            collection: &HardwareCollection,
        ) -> Option<accelerator_memory_observation::Result> {
            self.attribution
                .projection(
                    &desired(),
                    state,
                    Some(&native_target(Vec::new())),
                    collection,
                )
                .result
        }
    }

    type PortCall = (String, String, Option<HostProcessIdentity>);

    /// Runtime port reporting a settable figure for whatever process it is asked about, recording
    /// each call; it can hold one read until released.
    #[derive(Default)]
    struct SettablePort {
        bytes: std::sync::atomic::AtomicU64,
        calls: Mutex<Vec<PortCall>>,
        gate: Mutex<Option<Arc<PortGate>>>,
    }

    #[derive(Default)]
    struct PortGate {
        reached: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    impl SettablePort {
        fn set_bytes(&self, bytes: u64) {
            self.bytes.store(bytes, std::sync::atomic::Ordering::SeqCst);
        }

        fn calls(&self) -> Vec<PortCall> {
            self.calls.lock().expect("port calls").clone()
        }

        /// Holds the next read until the returned gate releases it.
        fn hold(&self) -> Arc<PortGate> {
            let gate = Arc::new(PortGate::default());
            *self.gate.lock().expect("port gate") = Some(Arc::clone(&gate));
            gate
        }
    }

    impl ProcessMemoryPort for SettablePort {
        fn observe<'a>(
            &'a self,
            execution_target_id: &'a str,
            workload_id: &'a WorkloadId,
            expected_process: Option<&'a HostProcessIdentity>,
        ) -> Pin<Box<dyn Future<Output = RuntimeMemoryReading> + Send + 'a>> {
            self.calls.lock().expect("port calls").push((
                execution_target_id.to_owned(),
                workload_id.0.clone(),
                expected_process.cloned(),
            ));
            let gate = self.gate.lock().expect("port gate").take();
            let bytes = self.bytes.load(std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.reached.notify_one();
                    gate.release.notified().await;
                }
                RuntimeMemoryReading::Available {
                    bytes,
                    observed_at: UNIX_EPOCH + Duration::from_secs(30),
                }
            })
        }
    }

    fn running(process_id: u32, process_start_token: u64) -> WorkloadObservation {
        WorkloadObservation::Running(workload(
            "node-a",
            "workload-a",
            process_id,
            process_start_token,
        ))
    }

    fn pending_memory() -> Option<accelerator_memory_observation::Result> {
        Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: "WORKLOAD_PROCESS_MEMORY_PENDING".to_owned(),
                message: "The runtime has not yet reported workload process memory.".to_owned(),
            },
        ))
    }

    fn native_target(accelerator_device_ids: Vec<String>) -> ExecutionTarget {
        ExecutionTarget {
            accelerator_device_ids,
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..target()
        }
    }

    pub(in crate::model::controller) fn desired() -> ModelDeploymentDeclaration {
        ModelDeploymentDeclaration {
            model_deployment_id: "deployment-a".to_owned(),
            placement: Some(dotcore_proto::dotcore::deployment::v1::ExactPlacement {
                site_node_id: "node-a".to_owned(),
                execution_target_id: "target-a".to_owned(),
            }),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(model_deployment_snapshot::Source::Local(Default::default())),
            }),
            ..Default::default()
        }
    }

    pub(in crate::model::controller) fn target() -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "target-a".to_owned(),
            display_name: "target-a".to_owned(),
            site_node_id: SiteNodeId::for_test("node-a").to_string(),
            accelerator_device_ids: vec!["nvidia.com/gpu=0".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                        dotcore_proto::dotcore::site::v1::DockerRuntimeAdapter::default(),
                    )),
                },
            )),
            platform: None,
        }
    }

    fn simulated_target() -> ExecutionTarget {
        ExecutionTarget {
            accelerator_device_ids: vec!["GPU-00000000-0000-0000-0000-000000005090".to_owned()],
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                dotcore_proto::dotcore::site::v1::ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    )),
                },
            )),
            ..target()
        }
    }

    fn state() -> ModelDeploymentState {
        ModelDeploymentState {
            status: ModelDeploymentStatus::Ready as i32,
            ..Default::default()
        }
    }

    pub(in crate::model::controller) fn identity(
        node_id: &str,
        process_id: u32,
        process_start_token: u64,
    ) -> HostProcessIdentity {
        HostProcessIdentity {
            managed_node_id: SiteNodeId::for_test(node_id),
            process_id,
            process_start_token,
        }
    }

    pub(in crate::model::controller) fn workload(
        node_id: &str,
        workload_id: &str,
        process_id: u32,
        process_start_token: u64,
    ) -> ObservedWorkload {
        ObservedWorkload {
            workload_id: WorkloadId(workload_id.to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "deployment-a".to_owned(),
                reconcile_generation: 1,
            },
            backend_id: RuntimeBackendId("docker".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: Some(identity(node_id, process_id, process_start_token)),
        }
    }

    pub(in crate::model::controller) fn observed_processes(
        processes: Vec<ObservedAcceleratorProcess>,
    ) -> AcceleratorProcessObservation {
        AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
            vendor: AcceleratorVendor::Nvidia,
            result: AcceleratorVendorProcessResult::Observed(processes),
        }])
    }

    pub(in crate::model::controller) fn process(
        device_id: &str,
        process_id: u32,
        process_start_token: u64,
        used_memory_bytes: Option<u64>,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            device_id: device_id.to_owned(),
            process_id,
            process_start_token,
            ancestors: Vec::new(),
            used_memory_bytes,
        }
    }

    fn child_process(
        device_id: &str,
        process_id: u32,
        process_start_token: u64,
        ancestors: &[(u32, u64)],
        used_memory_bytes: Option<u64>,
    ) -> ObservedAcceleratorProcess {
        ObservedAcceleratorProcess {
            ancestors: ancestors
                .iter()
                .map(|&(process_id, start_token)| ProcessLifetime {
                    process_id,
                    start_token,
                })
                .collect(),
            ..process(
                device_id,
                process_id,
                process_start_token,
                used_memory_bytes,
            )
        }
    }

    fn publish_node_a(
        publisher: &HardwareInventoryPublisher,
        processes: Vec<ObservedAcceleratorProcess>,
    ) {
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(10)),
                        ..Default::default()
                    },
                    accelerator_processes: observed_processes(processes),
                },
                timestamp(11),
            ),
        );
    }

    fn node_a_usage(devices: &[(&str, u64)]) -> Option<accelerator_memory_observation::Result> {
        Some(accelerator_memory_observation::Result::Available(
            AcceleratorMemoryUsageSet {
                devices: devices
                    .iter()
                    .map(
                        |&(device_id, used_memory_bytes)| AcceleratorMemoryDeviceUsage {
                            managed_node_id: SiteNodeId::for_test("node-a").to_string(),
                            device_id: device_id.to_owned(),
                            used_memory_bytes,
                        },
                    )
                    .collect(),
            },
        ))
    }

    pub(in crate::model::controller) fn timestamp(seconds: i64) -> Timestamp {
        Timestamp { seconds, nanos: 0 }
    }
}
