//! Records the accelerator memory a node attributes to each model deployment it hosts as metric
//! history, at the start of every wall-clock second.
//!
//! Each second re-sends the latest attribution the deployment's status reports, so history and
//! the current value never diverge and change in steps as the node refreshes its observations.
//! An attribution that is unavailable, not applicable, or too old to trust records nothing, so
//! history shows a gap rather than a held stale value.

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_domain::clock::Clock;
use dotcore_observability::{
    MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricRecord, MetricRecorder,
    MetricUnit,
    labels::{self, LabelSet},
};
use dotcore_proto::dotcore::deployment::v1::{
    AcceleratorMemoryObservation, accelerator_memory_observation,
};

/// Accelerator memory attributed to one model deployment on its hosting node, per device when the
/// node names one. The value is held from the latest attribution, which refreshes only at the
/// site's hardware refresh interval, so minimum and maximum over coarser buckets describe held
/// values rather than per-second measurements.
pub(crate) const DEPLOYMENT_ACCELERATOR_MEMORY_USED: &str = "deployment.accelerator.memory.used";

/// Writer name a node's deployment memory writer records under, the `source` of its
/// `node.observing` window.
pub(crate) const DEPLOYMENT_MEMORY_SOURCE: &str = "deployment_memory";

/// Time an observation may take to collect beyond twice the hardware refresh interval before its
/// attribution is too old to record.
pub(crate) const COLLECTION_ALLOWANCE: Duration = Duration::from_secs(5);

/// Interval at which a node re-sends the memory of the deployments it hosts.
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Declarations of the deployment memory metrics hosting nodes record.
pub(crate) fn deployment_metric_descriptors() -> Vec<MetricDescriptor> {
    vec![MetricDescriptor {
        name: DEPLOYMENT_ACCELERATOR_MEMORY_USED.to_owned(),
        kind: MetricKind::Gauge,
        unit: MetricUnit::Bytes,
        dimensions: vec![
            MetricDimension::required(labels::DEPLOYMENT),
            MetricDimension::required(labels::NODE),
            MetricDimension::optional(labels::DEVICE),
        ],
        provenance: MetricProvenance::Node,
        sample_interval: Some(SAMPLE_INTERVAL),
    }]
}

/// Current accelerator memory attribution of the model deployments one node hosts.
pub(crate) trait HostedAcceleratorMemory: Send + Sync {
    /// The attribution of each hosted deployment, by deployment id, as its status reports it.
    fn hosted_accelerator_memory(&self) -> Vec<(String, AcceleratorMemoryObservation)>;
}

/// Records the attributed accelerator memory of one node's hosted deployments every wall-clock
/// second into a metric recorder.
pub(crate) struct DeploymentMemoryWriter {
    node_id: String,
    hosted: Arc<dyn HostedAcceleratorMemory>,
    recorder: Arc<dyn MetricRecorder>,
    clock: Arc<dyn Clock>,

    /// Oldest attribution still recorded: twice the hardware refresh interval plus the collection
    /// allowance.
    max_age: Duration,
}

impl DeploymentMemoryWriter {
    /// Creates the writer of SiteNode `node_id` reading `hosted`, recording through `recorder` and
    /// scheduling on `clock`. Attributions older than twice `refresh_interval` plus
    /// `collection_allowance` record nothing.
    pub(crate) fn new(
        node_id: &str,
        hosted: Arc<dyn HostedAcceleratorMemory>,
        recorder: Arc<dyn MetricRecorder>,
        clock: Arc<dyn Clock>,
        refresh_interval: Duration,
        collection_allowance: Duration,
    ) -> Self {
        Self {
            node_id: node_id.to_owned(),
            hosted,
            recorder,
            clock,
            max_age: refresh_interval * 2 + collection_allowance,
        }
    }

    /// Records at the start of every wall-clock second until dropped; a second the writer missed
    /// stays absent.
    pub(crate) async fn run(self) {
        let mut last_second = None;
        loop {
            let into_second =
                Duration::from_nanos(u64::from(since_epoch(self.clock.wall_now()).subsec_nanos()));
            self.clock
                .sleep_until(self.clock.now() + (SAMPLE_INTERVAL - into_second))
                .await;
            let at = self.clock.wall_now();
            let second = since_epoch(at).as_secs();
            // Woken just before the boundary: wait for it rather than record a second twice.
            if last_second == Some(second) {
                continue;
            }
            last_second = Some(second);

            let records = self.records(at);
            if !records.is_empty() {
                self.recorder.record(records);
            }
        }
    }

    /// One record per device row of every hosted deployment whose attribution is available and
    /// fresh at `at`, timestamped `at`.
    fn records(&self, at: SystemTime) -> Vec<MetricRecord> {
        let mut records = vec![];
        for (deployment_id, memory) in self.hosted.hosted_accelerator_memory() {
            if !self.fresh(&memory, at) {
                continue;
            }
            let Some(accelerator_memory_observation::Result::Available(usage)) = memory.result
            else {
                continue;
            };
            let deployment = LabelSet::new()
                .with(labels::DEPLOYMENT, deployment_id)
                .with(labels::NODE, self.node_id.clone());
            for device in usage.devices {
                let labels = if device.device_id.is_empty() {
                    deployment.clone()
                } else {
                    deployment.clone().with(labels::DEVICE, device.device_id)
                };
                records.push(MetricRecord::gauge(
                    DEPLOYMENT_ACCELERATOR_MEMORY_USED,
                    labels,
                    at,
                    device.used_memory_bytes as f64,
                ));
            }
        }
        records
    }

    /// Whether `memory` was observed within the age bound of `at`. An observation after `at`, as
    /// when the wall clock stepped back, counts as fresh; one without a time never does.
    fn fresh(&self, memory: &AcceleratorMemoryObservation, at: SystemTime) -> bool {
        memory
            .observed_at
            .as_ref()
            .and_then(crate::metrics::system_time)
            .is_some_and(|observed_at| {
                at.duration_since(observed_at)
                    .map_or(true, |age| age <= self.max_age)
            })
    }
}

fn since_epoch(time: SystemTime) -> Duration {
    time.duration_since(UNIX_EPOCH).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use dotcore_domain::clock::VirtualClock;
    use dotcore_proto::dotcore::deployment::v1::{
        AcceleratorMemoryDeviceUsage, AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet,
    };
    use dotcore_proto::google::protobuf::Empty;
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn every_second_records_each_device_row_of_every_available_deployment() {
        let harness = Harness::start(vec![
            (
                "deployment-a",
                available(0, &[("GPU-a", 100), ("GPU-b", 200)]),
            ),
            ("deployment-b", available(0, &[("", 300)])),
        ]);

        for second in 1..=2 {
            harness.clock.advance_to_next_wake().await;
            assert_eq!(
                harness.next_records().await,
                vec![
                    record("deployment-a", Some("GPU-a"), second, 100.0),
                    record("deployment-a", Some("GPU-b"), second, 200.0),
                    record("deployment-b", None, second, 300.0),
                ]
            );
        }
    }

    #[tokio::test]
    async fn unavailable_not_applicable_and_stale_attributions_record_nothing() {
        // The first tick is at second 1, where the bound reaches back to second -14.
        let harness = Harness::start(vec![
            ("at-the-bound", available(-14, &[("GPU-a", 1)])),
            ("beyond-the-bound", available(-15, &[("GPU-a", 2)])),
            ("without-a-time", without_time(&[("GPU-a", 3)])),
            ("unavailable", unavailable()),
            ("not-applicable", not_applicable()),
        ]);

        harness.clock.advance_to_next_wake().await;

        assert_eq!(
            harness.next_records().await,
            vec![record("at-the-bound", Some("GPU-a"), 1, 1.0)]
        );
    }

    #[tokio::test]
    async fn an_attribution_observed_after_the_tick_records() {
        let harness = Harness::start(vec![("deployment-a", available(30, &[("GPU-a", 7)]))]);

        harness.clock.advance_to_next_wake().await;

        assert_eq!(
            harness.next_records().await,
            vec![record("deployment-a", Some("GPU-a"), 1, 7.0)]
        );
    }

    #[tokio::test]
    async fn a_deployment_no_longer_hosted_stops_recording() {
        let harness = Harness::start(vec![
            ("deployment-a", available(0, &[("GPU-a", 1)])),
            ("deployment-b", available(0, &[("GPU-a", 2)])),
        ]);
        harness.clock.advance_to_next_wake().await;
        assert_eq!(harness.next_records().await.len(), 2);

        harness.host(vec![("deployment-b", available(0, &[("GPU-a", 2)]))]);
        harness.clock.advance_to_next_wake().await;

        assert_eq!(
            harness.next_records().await,
            vec![record("deployment-b", Some("GPU-a"), 2, 2.0)]
        );
    }

    /// Whole seconds after the Unix epoch the virtual wall clock counts from.
    const EPOCH_SECONDS: u64 = 1_700_000_000;

    /// Hardware refresh interval of the harness writer; with the collection allowance, the age
    /// bound is 15 seconds.
    const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

    /// A writer over attributions the test sets, recording into a channel, on a virtual clock that
    /// starts a quarter second into second 0.
    struct Harness {
        clock: VirtualClock,
        hosted: Arc<FixedHosted>,
        records: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<MetricRecord>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        fn start(hosted: Vec<(&str, AcceleratorMemoryObservation)>) -> Self {
            let clock = VirtualClock::starting_at(
                UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS) + Duration::from_millis(250),
            );
            let hosted = Arc::new(FixedHosted(Mutex::new(owned(hosted))));
            let (sender, records) = mpsc::unbounded_channel();
            let writer = DeploymentMemoryWriter::new(
                "node-a",
                Arc::clone(&hosted) as Arc<dyn HostedAcceleratorMemory>,
                Arc::new(ChannelRecorder(sender)),
                Arc::new(clock.clone()),
                REFRESH_INTERVAL,
                COLLECTION_ALLOWANCE,
            );
            Self {
                clock,
                hosted,
                records: tokio::sync::Mutex::new(records),
                _task: tokio::spawn(writer.run()),
            }
        }

        /// Replaces the deployments the node hosts.
        fn host(&self, hosted: Vec<(&str, AcceleratorMemoryObservation)>) {
            *self.hosted.0.lock().expect("hosted deployments") = owned(hosted);
        }

        async fn next_records(&self) -> Vec<MetricRecord> {
            tokio::time::timeout(Duration::from_secs(10), self.records.lock().await.recv())
                .await
                .expect("writer progress deadline")
                .expect("writer running")
        }
    }

    fn owned(
        hosted: Vec<(&str, AcceleratorMemoryObservation)>,
    ) -> Vec<(String, AcceleratorMemoryObservation)> {
        hosted
            .into_iter()
            .map(|(deployment_id, memory)| (deployment_id.to_owned(), memory))
            .collect()
    }

    /// The record of `bytes` on `device` of `deployment` at second `second`.
    fn record(deployment: &str, device: Option<&str>, second: u64, bytes: f64) -> MetricRecord {
        let labels = LabelSet::new()
            .with("deployment", deployment)
            .with("node", "node-a");
        let labels = match device {
            Some(device) => labels.with("device", device),
            None => labels,
        };
        MetricRecord::gauge(
            DEPLOYMENT_ACCELERATOR_MEMORY_USED,
            labels,
            UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS + second),
            bytes,
        )
    }

    /// Usage of `devices` as (device id, bytes), observed at second `second`.
    fn available(second: i64, devices: &[(&str, u64)]) -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: Some(crate::metrics::timestamp(
                UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS.saturating_add_signed(second)),
            )),
            ..without_time(devices)
        }
    }

    fn without_time(devices: &[(&str, u64)]) -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: devices
                        .iter()
                        .map(
                            |&(device_id, used_memory_bytes)| AcceleratorMemoryDeviceUsage {
                                managed_node_id: "node-a".to_owned(),
                                device_id: device_id.to_owned(),
                                used_memory_bytes,
                            },
                        )
                        .collect(),
                },
            )),
        }
    }

    fn unavailable() -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "WORKLOAD_PROCESS_MEMORY_PENDING".to_owned(),
                    message: "pending".to_owned(),
                },
            )),
        }
    }

    fn not_applicable() -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: None,
            refresh: None,
            result: Some(accelerator_memory_observation::Result::NotApplicable(
                Empty {},
            )),
        }
    }

    /// Hosted deployments the test replaces between ticks.
    struct FixedHosted(Mutex<Vec<(String, AcceleratorMemoryObservation)>>);

    impl HostedAcceleratorMemory for FixedHosted {
        fn hosted_accelerator_memory(&self) -> Vec<(String, AcceleratorMemoryObservation)> {
            self.0.lock().expect("hosted deployments").clone()
        }
    }

    struct ChannelRecorder(mpsc::UnboundedSender<Vec<MetricRecord>>);

    impl MetricRecorder for ChannelRecorder {
        fn record(&self, records: Vec<MetricRecord>) {
            let _ = self.0.send(records);
        }
    }
}
