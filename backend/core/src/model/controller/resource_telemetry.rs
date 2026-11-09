//! Records workload resource telemetry for ready local model deployments: one
//! startup record when a realization first becomes ready, then a peak record
//! each time its attributed accelerator memory grows by at least 64 MiB.
//!
//! Each ready realization is sampled by its own background task, off the
//! reconciliation path, at the hardware refresh interval; a slow or failing
//! telemetry sink never delays or fails reconciliation. State is in memory,
//! so after a backend restart an already-ready realization records no startup
//! record and its first sample is recorded as a peak.

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_domain::{SiteNodeId, clock::Clock};
use dotcore_proto::{
    dotcore::{
        deployment::v1::{
            AcceleratorMemoryObservation, AcceleratorMemoryUsageSet, accelerator_memory_observation,
        },
        model::v1::{
            ModelDeploymentDeclaration, ModelDeploymentState, ModelDeploymentStatus,
            ResolvedLocalModelDeploymentSnapshot,
        },
        observation::v1::{
            AcceleratorFacts, accelerator_observation, host_observation::Result as HostResult,
        },
        site::v1::ExecutionTarget,
        telemetry::v1::{
            WorkloadRealizationKey, WorkloadResourceDevice, WorkloadResourceInputs,
            WorkloadResourceNode, WorkloadResourceObservation, WorkloadResourceObservationKind,
            WorkloadResourceRecord, WorkloadResourceSource,
            workload_resource_observation::EngineFacts,
        },
    },
    google::protobuf::{Duration as ProtoDuration, Timestamp},
};
use dotcore_runtime::{RuntimeLogSource, RuntimeWorkloadLogQuery, WorkloadId};
use dotcore_site::node::hardware_inventory::HardwareCollection;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::accelerator_memory::AttributionView;
use crate::{
    dispatch::model::RuntimeTargetMap,
    model::engine::{EngineIntegrationRegistry, startup_facts::read_startup_facts},
    telemetry::{TelemetrySink, new_workload_resource_record},
};

/// Minimum growth of attributed memory above the last recorded value that produces a peak record.
pub(super) const PEAK_GROWTH_BYTES: u64 = 64 * 1024 * 1024;

/// Engine startup facts resolved once for a realization's startup record.
pub(super) type EngineFactsFuture = Pin<Box<dyn Future<Output = Option<EngineFacts>> + Send>>;

/// Samples ready realizations, at most one task per deployment.
pub(super) struct WorkloadTelemetry {
    sink: Arc<dyn TelemetrySink>,
    samplers: Mutex<BTreeMap<String, RunningSampler>>,
}

impl WorkloadTelemetry {
    pub(super) fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            sink,
            samplers: Mutex::new(BTreeMap::new()),
        }
    }

    /// Reports whether this exact realization generation is already sampled.
    pub(super) fn samples(&self, deployment_id: &str, generation: u64) -> bool {
        self.samplers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(deployment_id)
            .is_some_and(|running| running.generation == generation)
    }

    /// Starts sampling a realization, replacing a sampler of an older generation.
    pub(super) fn start(&self, realization: ReadyRealization, view: AttributionView) {
        let deployment_id = realization.desired.model_deployment_id.clone();
        let mut samplers = self
            .samplers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if samplers
            .get(&deployment_id)
            .is_some_and(|running| running.generation == realization.generation)
        {
            return;
        }
        if let Some(previous) = samplers.remove(&deployment_id) {
            previous.cancellation.cancel();
        }
        let cancellation = CancellationToken::new();
        samplers.insert(
            deployment_id.clone(),
            RunningSampler {
                generation: realization.generation,
                cancellation: cancellation.clone(),
            },
        );
        debug!(
            event_kind = "workload_telemetry_started",
            deployment_id = %deployment_id,
            generation = realization.generation,
            first_ready = realization.first_ready,
            "workload resource telemetry sampling started"
        );
        tokio::spawn(sample(
            realization,
            view,
            Arc::clone(&self.sink),
            cancellation,
        ));
    }

    /// Stops sampling a deployment's realization.
    pub(super) fn stop(&self, deployment_id: &str) {
        if let Some(running) = self
            .samplers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(deployment_id)
        {
            running.cancellation.cancel();
        }
    }
}

impl Drop for WorkloadTelemetry {
    fn drop(&mut self) {
        for running in self
            .samplers
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
        {
            running.cancellation.cancel();
        }
    }
}

struct RunningSampler {
    generation: u64,
    cancellation: CancellationToken,
}

/// One ready realization and the inputs its records carry.
pub(super) struct ReadyRealization {
    pub(super) desired: ModelDeploymentDeclaration,
    pub(super) target: ExecutionTarget,
    pub(super) generation: u64,
    pub(super) inputs: WorkloadResourceInputs,
    pub(super) ready_at: SystemTime,
    /// False when the realization was already ready before this process observed it.
    pub(super) first_ready: bool,
    pub(super) interval: Duration,
    pub(super) engine_facts: EngineFactsFuture,

    /// Backend clock the samples are scheduled on.
    pub(super) clock: Arc<dyn Clock>,
}

/// Reads engine startup facts of a deployment's workload through its execution target's runtime.
pub(super) fn engine_facts_future(
    registry: Arc<EngineIntegrationRegistry>,
    runtimes: RuntimeTargetMap,
    snapshot: ResolvedLocalModelDeploymentSnapshot,
    execution_target_id: String,
    workload_id: WorkloadId,
) -> EngineFactsFuture {
    Box::pin(async move {
        let extractor = registry.startup_fact_extractor(&snapshot)?;
        let source = RuntimeLogSource {
            name: super::logs::MODEL_LOG_SOURCE_NAME.to_owned(),
            label: super::logs::MODEL_LOG_SOURCE_LABEL.to_owned(),
        };
        let (runtimes, execution_target_id, workload_id) =
            (&runtimes, &execution_target_id, &workload_id);
        let read = read_startup_facts(extractor, |history| {
            let query = RuntimeWorkloadLogQuery::new(source.clone(), history);
            async move {
                runtimes
                    .read_workload_logs(execution_target_id, workload_id, &query)
                    .await
            }
        })
        .await
        .inspect_err(|error| {
            warn!(
                event_kind = "workload_telemetry_engine_facts_unavailable",
                execution_target_id = %execution_target_id,
                workload_id = %workload_id.0,
                error = %error,
                "engine startup facts could not be read"
            );
        })
        .ok()
        .flatten()?;
        // Lines a log store dropped never come back, so an incomplete read is reported, not retried.
        if !read.extracted.complete() {
            warn!(
                event_kind = "workload_telemetry_engine_facts_incomplete",
                execution_target_id = %execution_target_id,
                workload_id = %workload_id.0,
                missing = ?read.extracted.missing,
                read_end = ?read.end,
                skipped_crowded_second = read.skipped_crowded_second,
                "engine startup facts are incomplete"
            );
        }
        Some(read.extracted.facts)
    })
}

async fn sample(
    realization: ReadyRealization,
    view: AttributionView,
    sink: Arc<dyn TelemetrySink>,
    cancellation: CancellationToken,
) {
    let ReadyRealization {
        desired,
        target,
        generation,
        inputs,
        ready_at,
        first_ready,
        interval,
        engine_facts,
        clock,
    } = realization;
    // Facts are read once at readiness, before request logs push the startup lines out of
    // history, concurrently with sampling so a slow read cannot delay the startup sample.
    let mut pending_facts = first_ready.then_some(engine_facts);
    let mut engine_facts = None;
    let mut sampler = Sampler {
        realization: WorkloadRealizationKey {
            owner_id: desired.model_deployment_id.clone(),
            generation,
        },
        inputs,
        ready_at,
        startup_pending: first_ready,
        last_observed_at: None,
        last_recorded_bytes: None,
        unavailable_reported: false,
    };
    let ready = ModelDeploymentState {
        status: ModelDeploymentStatus::Ready as i32,
        ..ModelDeploymentState::default()
    };
    let mut next_sample = clock.now() + interval;
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return,
            facts = async { pending_facts.as_mut().expect("pending facts").await },
                if pending_facts.is_some() =>
            {
                engine_facts = facts;
                pending_facts = None;
                continue;
            }
            () = clock.sleep_until(next_sample) => {}
        }
        next_sample += interval;
        // One collection for the attribution and the node facts recorded with it.
        let collection = view.collection();
        let observation = view.projection(&desired, &ready, Some(&target), &collection);
        let Some(mut record) = sampler.observe(&observation, &collection, clock.wall_now()) else {
            continue;
        };
        if let Some(observation) = record.observation.as_mut()
            && observation.kind == WorkloadResourceObservationKind::Startup as i32
        {
            if let Some(facts) = pending_facts.take() {
                engine_facts = tokio::select! {
                    () = cancellation.cancelled() => return,
                    facts = facts => facts,
                };
            }
            observation.engine_facts = engine_facts.take();
        }
        let sink = Arc::clone(&sink);
        // Appending is blocking file I/O, kept off the async executor.
        let _ = tokio::task::spawn_blocking(move || sink.record_workload_resource(&record)).await;
    }
}

/// Decides which samples become records for one realization.
struct Sampler {
    realization: WorkloadRealizationKey,
    inputs: WorkloadResourceInputs,
    ready_at: SystemTime,
    startup_pending: bool,
    last_observed_at: Option<SystemTime>,
    last_recorded_bytes: Option<u64>,
    unavailable_reported: bool,
}

impl Sampler {
    fn observe(
        &mut self,
        observation: &AcceleratorMemoryObservation,
        collection: &HardwareCollection,
        now: SystemTime,
    ) -> Option<WorkloadResourceRecord> {
        let Some(accelerator_memory_observation::Result::Available(usage)) = &observation.result
        else {
            if !self.unavailable_reported {
                self.unavailable_reported = true;
                debug!(
                    event_kind = "workload_telemetry_sample_unavailable",
                    deployment_id = %self.realization.owner_id,
                    reason = ?observation.result,
                    "workload accelerator memory is unavailable for telemetry"
                );
            }
            return None;
        };
        let observed_at = observation.observed_at.as_ref().and_then(system_time)?;
        if observed_at <= self.ready_at
            || self
                .last_observed_at
                .is_some_and(|last| observed_at <= last)
        {
            return None;
        }
        self.last_observed_at = Some(observed_at);
        let total = usage
            .devices
            .iter()
            .map(|device| device.used_memory_bytes)
            .sum::<u64>();
        let kind = if self.startup_pending {
            WorkloadResourceObservationKind::Startup
        } else if self
            .last_recorded_bytes
            .is_none_or(|last| total >= last.saturating_add(PEAK_GROWTH_BYTES))
        {
            WorkloadResourceObservationKind::Peak
        } else {
            return None;
        };
        self.startup_pending = false;
        self.last_recorded_bytes = Some(total);
        Some(self.record(kind, observed_at, total, usage, collection, now))
    }

    fn record(
        &self,
        kind: WorkloadResourceObservationKind,
        observed_at: SystemTime,
        total: u64,
        usage: &AcceleratorMemoryUsageSet,
        collection: &HardwareCollection,
        now: SystemTime,
    ) -> WorkloadResourceRecord {
        let since_ready = observed_at
            .duration_since(self.ready_at)
            .unwrap_or_default();
        WorkloadResourceRecord {
            realization: Some(self.realization.clone()),
            inputs: Some(self.inputs.clone()),
            node: Some(node(usage, collection)),
            observation: Some(WorkloadResourceObservation {
                kind: kind.into(),
                since_ready: Some(ProtoDuration {
                    seconds: since_ready.as_secs().try_into().unwrap_or(i64::MAX),
                    nanos: since_ready.subsec_nanos() as i32,
                }),
                attributed_memory_bytes: total,
                engine_facts: None,
            }),
            ..new_workload_resource_record(WorkloadResourceSource::Deployment, now)
        }
    }
}

/// Describes the node and devices of the attributed usage from the hardware facts of the
/// collection it was attributed from.
fn node(
    usage: &AcceleratorMemoryUsageSet,
    collection: &HardwareCollection,
) -> WorkloadResourceNode {
    let node_id = usage
        .devices
        .first()
        .map(|device| device.managed_node_id.clone())
        .unwrap_or_default();
    let snapshot = SiteNodeId::parse(&node_id)
        .ok()
        .and_then(|node| collection.entry(&node))
        .and_then(|entry| entry.snapshot.clone());
    let host = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.host.as_ref())
        .and_then(|host| match &host.result {
            Some(HostResult::Facts(facts)) => Some(facts),
            _ => None,
        });
    let accelerators = snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .accelerators
                .iter()
                .filter_map(|observation| match &observation.result {
                    Some(accelerator_observation::Result::Facts(set)) => Some(set.facts.iter()),
                    _ => None,
                })
                .flatten()
                .collect::<Vec<&AcceleratorFacts>>()
        })
        .unwrap_or_default();
    WorkloadResourceNode {
        node_id: node_id.clone(),
        host_total_memory_bytes: host.map(|facts| facts.total_memory_bytes),
        host_available_memory_bytes: host.map(|facts| facts.available_memory_bytes),
        devices: usage
            .devices
            .iter()
            .map(|device| {
                let facts = accelerators
                    .iter()
                    .find(|facts| facts.device_id == device.device_id);
                WorkloadResourceDevice {
                    device_id: device.device_id.clone(),
                    model: facts.map(|facts| facts.model.clone()).unwrap_or_default(),
                    driver_version: facts
                        .map(|facts| facts.driver_or_runtime_version.clone())
                        .unwrap_or_default(),
                    memory_architecture: facts
                        .map(|facts| facts.memory_architecture)
                        .unwrap_or_default(),
                    total_memory_bytes: facts.and_then(|facts| facts.total_memory_bytes),
                    attributed_memory_bytes: device.used_memory_bytes,
                    // Deployment usage rows cover the workload only, so other processes are unobserved.
                    unattributed_peak_memory_bytes: None,
                }
            })
            .collect(),
    }
}

fn system_time(timestamp: &Timestamp) -> Option<SystemTime> {
    let seconds = u64::try_from(timestamp.seconds).ok()?;
    let nanos = u32::try_from(timestamp.nanos).ok()?;
    UNIX_EPOCH.checked_add(Duration::new(seconds, nanos))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Condvar,
        atomic::{AtomicUsize, Ordering},
    };

    use dotcore_domain::{clock::SystemClock, hardware_observation::NodeHardwareObservation};
    use dotcore_proto::dotcore::{
        observation::v1::{
            AcceleratorFactsSet, AcceleratorMemoryArchitecture, AcceleratorObservation,
            AcceleratorVendor, HardwareSnapshot, HostFacts, HostObservation,
        },
        telemetry::v1::VllmStartupFacts,
    };
    use dotcore_runtime::{
        ObservedWorkload, RuntimeBackendId, WorkloadCorrelation, WorkloadObservation,
    };
    use dotcore_site::node::hardware_inventory::{
        HardwareInventoryEntry, HardwareInventoryPublisher, HardwareInventorySource,
    };

    use super::*;
    use crate::model::controller::accelerator_memory::{
        AcceleratorAttribution,
        tests::{desired, identity, observed_processes, process, target, timestamp},
    };

    const MIB: u64 = 1024 * 1024;

    #[test]
    fn no_record_before_a_snapshot_newer_than_ready() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let mut sampler = sampler(true);
        publish(&publisher, 100, 1_000 * MIB);

        assert_eq!(sample(&mut sampler, &inventory), None);
    }

    #[test]
    fn records_startup_then_only_growth_beyond_the_threshold() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let mut sampler = sampler(true);

        publish(&publisher, 101, 1_000 * MIB);
        let startup = sample(&mut sampler, &inventory).expect("startup record");
        assert_eq!(
            startup.observation,
            Some(WorkloadResourceObservation {
                kind: WorkloadResourceObservationKind::Startup.into(),
                since_ready: Some(ProtoDuration {
                    seconds: 1,
                    nanos: 0,
                }),
                attributed_memory_bytes: 1_000 * MIB,
                engine_facts: None,
            })
        );
        assert_eq!(startup.source, WorkloadResourceSource::Deployment as i32);
        assert_eq!(
            startup.realization,
            Some(WorkloadRealizationKey {
                owner_id: "deployment-a".to_owned(),
                generation: 3,
            })
        );
        assert_eq!(startup.inputs, Some(inputs()));
        assert_eq!(
            startup.node,
            Some(WorkloadResourceNode {
                node_id: SiteNodeId::for_test("node-a").to_string(),
                host_total_memory_bytes: Some(64_000 * MIB),
                host_available_memory_bytes: Some(40_000 * MIB),
                devices: vec![WorkloadResourceDevice {
                    device_id: "GPU-a".to_owned(),
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    driver_version: "580.95".to_owned(),
                    memory_architecture: AcceleratorMemoryArchitecture::Dedicated.into(),
                    total_memory_bytes: Some(32_000 * MIB),
                    attributed_memory_bytes: 1_000 * MIB,
                    unattributed_peak_memory_bytes: None,
                }],
            })
        );

        assert_eq!(sample(&mut sampler, &inventory), None, "same snapshot");
        publish(&publisher, 102, 1_063 * MIB);
        assert_eq!(
            sample(&mut sampler, &inventory),
            None,
            "growth under 64 MiB"
        );
        publish(&publisher, 103, 1_064 * MIB);
        let peak = sample(&mut sampler, &inventory).expect("peak record");
        let observation = peak.observation.expect("observation");
        assert_eq!(
            observation.kind,
            WorkloadResourceObservationKind::Peak as i32
        );
        assert_eq!(observation.attributed_memory_bytes, 1_064 * MIB);
        assert_eq!(observation.engine_facts, None);
        assert_ne!(peak.record_id, startup.record_id);
    }

    #[test]
    fn restarted_realization_records_its_first_sample_as_a_peak() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        let mut sampler = sampler(false);
        publish(&publisher, 101, 1_000 * MIB);

        let record = sample(&mut sampler, &inventory).expect("peak record");

        assert_eq!(
            record.observation.expect("observation").kind,
            WorkloadResourceObservationKind::Peak as i32
        );
    }

    #[test]
    fn node_without_fresh_inventory_records_nothing() {
        let (_publisher, inventory) = HardwareInventoryPublisher::new();
        let mut sampler = sampler(true);

        assert_eq!(sample(&mut sampler, &inventory), None);
        assert!(sampler.unavailable_reported);
    }

    #[tokio::test]
    async fn blocking_sink_does_not_delay_ready_and_records_in_the_background() {
        let (publisher, inventory) = HardwareInventoryPublisher::new();
        publish(&publisher, 101, 1_000 * MIB);
        let sink = Arc::new(BlockingSink::default());
        let attribution = AcceleratorAttribution::new(Arc::new(inventory))
            .with_telemetry(Some(sink.clone() as Arc<dyn TelemetrySink>));
        attribution.observe("deployment-a", None, &running());

        attribution.ready("deployment-a", 3, || Some(realization(true)));
        attribution.ready("deployment-a", 3, || panic!("already sampled generation"));
        for _ in 0..200 {
            if sink.calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(sink.calls.load(Ordering::SeqCst), 1);
        let observation = sink.records.lock().expect("records")[0]
            .observation
            .expect("observation");
        assert_eq!(
            observation.kind,
            WorkloadResourceObservationKind::Startup as i32
        );
        assert_eq!(observation.engine_facts, Some(facts()));
        attribution.forget("deployment-a");
        sink.release();
    }

    fn sample(
        sampler: &mut Sampler,
        inventory: &dyn HardwareInventorySource,
    ) -> Option<WorkloadResourceRecord> {
        let collection = inventory.collection();
        let view_projection = crate::model::controller::accelerator_memory::tests::project_for_test(
            &collection,
            &std::collections::BTreeSet::from([identity("node-a", 41, 700)]),
        );
        sampler.observe(&view_projection, &collection, SystemTime::now())
    }

    fn sampler(first_ready: bool) -> Sampler {
        Sampler {
            realization: WorkloadRealizationKey {
                owner_id: "deployment-a".to_owned(),
                generation: 3,
            },
            inputs: inputs(),
            ready_at: UNIX_EPOCH + Duration::from_secs(100),
            startup_pending: first_ready,
            last_observed_at: None,
            last_recorded_bytes: None,
            unavailable_reported: false,
        }
    }

    fn realization(first_ready: bool) -> ReadyRealization {
        ReadyRealization {
            desired: desired(),
            target: target(),
            generation: 3,
            inputs: inputs(),
            ready_at: UNIX_EPOCH + Duration::from_secs(100),
            first_ready,
            interval: Duration::from_millis(5),
            engine_facts: Box::pin(async { Some(facts()) }),
            clock: Arc::new(SystemClock),
        }
    }

    fn running() -> WorkloadObservation {
        WorkloadObservation::Running(ObservedWorkload {
            workload_id: WorkloadId("workload-a".to_owned()),
            correlation: WorkloadCorrelation {
                owner_id: "deployment-a".to_owned(),
                reconcile_generation: 3,
            },
            backend_id: RuntimeBackendId("docker".to_owned()),
            loopback_endpoints: Vec::new(),
            services: Vec::new(),
            host_process: Some(identity("node-a", 41, 700)),
        })
    }

    fn inputs() -> WorkloadResourceInputs {
        WorkloadResourceInputs {
            model_id: "qwen3-8-27b".to_owned(),
            engine_release_id: "vllm".to_owned(),
            ..WorkloadResourceInputs::default()
        }
    }

    fn facts() -> EngineFacts {
        EngineFacts::Vllm(VllmStartupFacts {
            extractor_version: 1,
            kv_cache_token_capacity: Some(172_480),
            ..VllmStartupFacts::default()
        })
    }

    fn publish(publisher: &HardwareInventoryPublisher, observed_at: i64, bytes: u64) {
        publisher.publish(
            &SiteNodeId::for_test("node-a"),
            HardwareInventoryEntry::succeeded(
                NodeHardwareObservation {
                    snapshot: HardwareSnapshot {
                        observed_at: Some(timestamp(observed_at)),
                        host: Some(HostObservation {
                            result: Some(HostResult::Facts(HostFacts {
                                total_memory_bytes: 64_000 * MIB,
                                available_memory_bytes: 40_000 * MIB,
                                ..HostFacts::default()
                            })),
                        }),
                        accelerators: vec![AcceleratorObservation {
                            vendor: AcceleratorVendor::Nvidia.into(),
                            result: Some(accelerator_observation::Result::Facts(
                                AcceleratorFactsSet {
                                    facts: vec![AcceleratorFacts {
                                        vendor: AcceleratorVendor::Nvidia.into(),
                                        model: "NVIDIA GeForce RTX 5090".to_owned(),
                                        memory_architecture:
                                            AcceleratorMemoryArchitecture::Dedicated.into(),
                                        total_memory_bytes: Some(32_000 * MIB),
                                        driver_or_runtime_version: "580.95".to_owned(),
                                        device_id: "GPU-a".to_owned(),
                                        vendor_index: Some(0),
                                        used_memory_bytes: None,
                                        working_set_limit_bytes: None,
                                        supported_signals: Vec::new(),
                                    }],
                                },
                            )),
                        }],
                    },
                    accelerator_processes: observed_processes(vec![process(
                        "GPU-a",
                        41,
                        700,
                        Some(bytes),
                    )]),
                },
                timestamp(observed_at + 1),
            ),
        );
    }

    /// Sink that keeps every record and blocks each write until released.
    #[derive(Default)]
    struct BlockingSink {
        calls: AtomicUsize,
        records: Mutex<Vec<WorkloadResourceRecord>>,
        released: Mutex<bool>,
        condition: Condvar,
    }

    impl BlockingSink {
        fn release(&self) {
            *self.released.lock().expect("release lock") = true;
            self.condition.notify_all();
        }
    }

    impl TelemetrySink for BlockingSink {
        fn record_workload_resource(&self, record: &WorkloadResourceRecord) {
            self.records.lock().expect("records").push(record.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            let released = self.released.lock().expect("release lock");
            let _released = self
                .condition
                .wait_while(released, |released| !*released)
                .expect("release wait");
        }
    }
}
