//! Records the host CPU, host memory and accelerator memory a node attributes to each service of
//! the application deployments it hosts as metric history, at the start of every wall-clock
//! second.
//!
//! Each second re-sends what the node's application status attributes, so history and the
//! current value never diverge and change in steps as the node refreshes its observations. A
//! figure that is unavailable or too old to trust records nothing, so history shows a gap rather
//! than a held stale value.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
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

use super::resources::ServiceHostResources;
use crate::metric_resend::{Freshness, RESEND_INTERVAL, resend_every_second};

/// Share, from 0 to 1, of the hosting machine's logical CPUs one application service kept busy
/// between the node's two latest readings of it.
pub(crate) const APPLICATION_HOST_CPU_UTILIZATION: &str = "application.host.cpu.utilization";

/// Host memory one application service's processes hold, not counting reclaimable file cache.
pub(crate) const APPLICATION_HOST_MEMORY_USED: &str = "application.host.memory.used";

/// Accelerator memory attributed to one application service on its hosting node, per device when
/// the node names one.
pub(crate) const APPLICATION_ACCELERATOR_MEMORY_USED: &str = "application.accelerator.memory.used";

/// Writer name a node's application resource writer records under, the `source` of its
/// `node.observing` window.
pub(crate) const APPLICATION_RESOURCES_SOURCE: &str = "application_resources";

/// Declarations of the application resource metrics hosting nodes record. Every value is held
/// from the latest hardware collection, so minimum and maximum over coarser buckets describe held
/// values rather than per-second measurements.
pub(crate) fn application_metric_descriptors() -> Vec<MetricDescriptor> {
    let gauge = |name: &str, unit, device: bool| {
        let mut dimensions = vec![
            MetricDimension::required(labels::DEPLOYMENT),
            MetricDimension::required(labels::SERVICE),
            MetricDimension::required(labels::NODE),
        ];
        if device {
            dimensions.push(MetricDimension::optional(labels::DEVICE));
        }
        MetricDescriptor {
            name: name.to_owned(),
            kind: MetricKind::Gauge,
            unit,
            dimensions,
            provenance: MetricProvenance::Node,
            sample_interval: Some(RESEND_INTERVAL),
        }
    };
    vec![
        gauge(APPLICATION_HOST_CPU_UTILIZATION, MetricUnit::Ratio, false),
        gauge(APPLICATION_HOST_MEMORY_USED, MetricUnit::Bytes, false),
        gauge(APPLICATION_ACCELERATOR_MEMORY_USED, MetricUnit::Bytes, true),
    ]
}

/// Current resource attribution of the application deployments one node hosts.
pub(crate) trait HostedApplicationResources: Send + Sync {
    /// Every hosted deployment's services, as the node's application status attributes them.
    fn hosted_application_resources(&self) -> Vec<HostedApplication>;
}

/// One hosted application deployment and the resources of each of its services.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HostedApplication {
    pub(crate) deployment_id: String,
    pub(crate) services: Vec<HostedService>,
}

/// The resources one service of a hosted application deployment uses.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HostedService {
    pub(crate) service_name: String,

    /// Host CPU and memory, when the runtime gave figures for the service.
    pub(crate) host: Option<ServiceHostResources>,

    /// Accelerator memory, exactly as the status reports it.
    pub(crate) accelerator_memory: Option<AcceleratorMemoryObservation>,
}

/// Records the attributed resources of one node's hosted application services every wall-clock
/// second into a metric recorder.
pub(crate) struct ApplicationResourceWriter {
    node_id: String,
    hosted: Arc<dyn HostedApplicationResources>,
    recorder: Arc<dyn MetricRecorder>,
    clock: Arc<dyn Clock>,
    freshness: Freshness,
}

impl ApplicationResourceWriter {
    /// Creates the writer of SiteNode `node_id` reading `hosted`, recording through `recorder` and
    /// scheduling on `clock`. Figures older than twice `refresh_interval` plus
    /// `collection_allowance` record nothing.
    pub(crate) fn new(
        node_id: &str,
        hosted: Arc<dyn HostedApplicationResources>,
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
            freshness: Freshness::new(refresh_interval, collection_allowance),
        }
    }

    /// Records at the start of every wall-clock second until dropped; a second the writer missed
    /// stays absent.
    pub(crate) async fn run(self) {
        resend_every_second(self.clock.as_ref(), self.recorder.as_ref(), |at| {
            self.records(at)
        })
        .await;
    }

    /// The records of every hosted service's fresh figures at `at`, timestamped `at`: its CPU
    /// share and host memory, and one accelerator memory record per device row.
    fn records(&self, at: SystemTime) -> Vec<MetricRecord> {
        let mut records = vec![];
        for application in self.hosted.hosted_application_resources() {
            for service in application.services {
                let labels = LabelSet::new()
                    .with(labels::DEPLOYMENT, application.deployment_id.clone())
                    .with(labels::SERVICE, service.service_name)
                    .with(labels::NODE, self.node_id.clone());
                if let Some(host) = service
                    .host
                    .filter(|host| self.freshness.admits(Some(host.observed_at), at))
                {
                    if let Some(cpu) = host.cpu_utilization {
                        records.push(MetricRecord::gauge(
                            APPLICATION_HOST_CPU_UTILIZATION,
                            labels.clone(),
                            at,
                            cpu,
                        ));
                    }
                    records.push(MetricRecord::gauge(
                        APPLICATION_HOST_MEMORY_USED,
                        labels.clone(),
                        at,
                        host.memory_bytes as f64,
                    ));
                }
                if let Some(memory) = service.accelerator_memory {
                    self.accelerator_records(&labels, memory, at, &mut records);
                }
            }
        }
        records
    }

    /// Appends one record per device row of `memory` when it is available and fresh at `at`.
    fn accelerator_records(
        &self,
        service: &LabelSet,
        memory: AcceleratorMemoryObservation,
        at: SystemTime,
        records: &mut Vec<MetricRecord>,
    ) {
        let observed_at = memory
            .observed_at
            .as_ref()
            .and_then(crate::metrics::system_time);
        if !self.freshness.admits(observed_at, at) {
            return;
        }
        let Some(accelerator_memory_observation::Result::Available(usage)) = memory.result else {
            return;
        };
        for device in usage.devices {
            let labels = if device.device_id.is_empty() {
                service.clone()
            } else {
                service.clone().with(labels::DEVICE, device.device_id)
            };
            records.push(MetricRecord::gauge(
                APPLICATION_ACCELERATOR_MEMORY_USED,
                labels,
                at,
                device.used_memory_bytes as f64,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use dotcore_domain::clock::VirtualClock;
    use dotcore_proto::dotcore::deployment::v1::{
        AcceleratorMemoryDeviceUsage, AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet,
    };
    use tokio::sync::mpsc;

    use super::*;
    use crate::metric_resend::COLLECTION_ALLOWANCE;

    #[tokio::test]
    async fn every_second_re_sends_each_services_cpu_memory_and_device_rows() {
        let harness = Harness::start(vec![application(
            "app-1",
            vec![
                service("cache", Some(host(0, 2048, None)), None),
                service(
                    "web",
                    Some(host(0, 4096, Some(0.25))),
                    Some(accelerator(0, &[("GPU-a", 100), ("", 300)])),
                ),
            ],
        )]);

        for second in 1..=2 {
            harness.clock.advance_to_next_wake().await;
            assert_eq!(
                harness.next_records().await,
                vec![
                    record(APPLICATION_HOST_MEMORY_USED, "cache", None, second, 2048.0),
                    record(APPLICATION_HOST_CPU_UTILIZATION, "web", None, second, 0.25),
                    record(APPLICATION_HOST_MEMORY_USED, "web", None, second, 4096.0),
                    record(
                        APPLICATION_ACCELERATOR_MEMORY_USED,
                        "web",
                        Some("GPU-a"),
                        second,
                        100.0
                    ),
                    record(
                        APPLICATION_ACCELERATOR_MEMORY_USED,
                        "web",
                        None,
                        second,
                        300.0
                    ),
                ]
            );
        }
    }

    #[tokio::test]
    async fn services_without_figures_or_past_the_freshness_bound_record_nothing() {
        // The first tick is at second 1, where the bound reaches back to second -14.
        let harness = Harness::start(vec![application(
            "app-1",
            vec![
                service("without-figures", None, Some(unavailable())),
                service(
                    "at-the-bound",
                    Some(host(-14, 1, Some(0.5))),
                    Some(accelerator(-14, &[("GPU-a", 2)])),
                ),
                service(
                    "beyond-the-bound",
                    Some(host(-15, 3, Some(0.5))),
                    Some(accelerator(-15, &[("GPU-a", 4)])),
                ),
            ],
        )]);

        harness.clock.advance_to_next_wake().await;

        assert_eq!(
            harness.next_records().await,
            vec![
                record(
                    APPLICATION_HOST_CPU_UTILIZATION,
                    "at-the-bound",
                    None,
                    1,
                    0.5
                ),
                record(APPLICATION_HOST_MEMORY_USED, "at-the-bound", None, 1, 1.0),
                record(
                    APPLICATION_ACCELERATOR_MEMORY_USED,
                    "at-the-bound",
                    Some("GPU-a"),
                    1,
                    2.0
                ),
            ]
        );
    }

    #[tokio::test]
    async fn values_stop_once_the_held_figures_pass_the_freshness_bound() {
        let harness = Harness::start(vec![application(
            "app-1",
            vec![service("web", Some(host(0, 7, Some(0.5))), None)],
        )]);
        harness.clock.advance_to_next_wake().await;
        assert_eq!(harness.next_records().await.len(), 2);

        // Figures observed at second 0 stay fresh through second 15, then stop.
        let mut last_recorded = 1;
        loop {
            harness.clock.advance_to_next_wake().await;
            let second = harness.second();
            if second > 15 {
                break;
            }
            assert_eq!(
                harness.next_records().await,
                vec![
                    record(APPLICATION_HOST_CPU_UTILIZATION, "web", None, second, 0.5),
                    record(APPLICATION_HOST_MEMORY_USED, "web", None, second, 7.0),
                ]
            );
            last_recorded = second;
        }
        assert_eq!(last_recorded, 15);

        // The writer waits for second 17 only once it handled second 16, which sent nothing.
        harness.clock.advance_to_next_wake().await;
        assert_eq!(
            harness.records.lock().await.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        );
    }

    /// Whole seconds after the Unix epoch the virtual wall clock counts from.
    const EPOCH_SECONDS: u64 = 1_700_000_000;

    /// Hardware refresh interval of the harness writer; with the collection allowance, the age
    /// bound is 15 seconds.
    const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

    /// A writer over fixed hosted applications, recording into a channel, on a virtual
    /// clock that starts a quarter second into second 0.
    struct Harness {
        clock: VirtualClock,
        records: tokio::sync::Mutex<mpsc::UnboundedReceiver<Vec<MetricRecord>>>,
        _task: tokio::task::JoinHandle<()>,
    }

    impl Harness {
        fn start(hosted: Vec<HostedApplication>) -> Self {
            let clock = VirtualClock::starting_at(
                UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS) + Duration::from_millis(250),
            );
            let hosted = Arc::new(FixedHosted(hosted));
            let (sender, records) = mpsc::unbounded_channel();
            let writer = ApplicationResourceWriter::new(
                "node-a",
                hosted as Arc<dyn HostedApplicationResources>,
                Arc::new(ChannelRecorder(sender)),
                Arc::new(clock.clone()),
                REFRESH_INTERVAL,
                COLLECTION_ALLOWANCE,
            );
            Self {
                clock,
                records: tokio::sync::Mutex::new(records),
                _task: tokio::spawn(writer.run()),
            }
        }

        /// Seconds since the harness epoch on the virtual wall clock.
        fn second(&self) -> u64 {
            self.clock
                .wall_now()
                .duration_since(UNIX_EPOCH)
                .expect("wall time after the epoch")
                .as_secs()
                - EPOCH_SECONDS
        }

        async fn next_records(&self) -> Vec<MetricRecord> {
            tokio::time::timeout(Duration::from_secs(10), self.records.lock().await.recv())
                .await
                .expect("writer progress deadline")
                .expect("writer running")
        }
    }

    /// The record of `value` for `service` of `app-1`, on `device` when given, at second
    /// `second`.
    fn record(
        metric: &str,
        service: &str,
        device: Option<&str>,
        second: u64,
        value: f64,
    ) -> MetricRecord {
        let labels = LabelSet::new()
            .with("deployment", "app-1")
            .with("service", service)
            .with("node", "node-a");
        let labels = match device {
            Some(device) => labels.with("device", device),
            None => labels,
        };
        MetricRecord::gauge(metric, labels, at(second as i64), value)
    }

    fn at(second: i64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(EPOCH_SECONDS.saturating_add_signed(second))
    }

    fn application(deployment_id: &str, services: Vec<HostedService>) -> HostedApplication {
        HostedApplication {
            deployment_id: deployment_id.to_owned(),
            services,
        }
    }

    fn service(
        service_name: &str,
        host: Option<ServiceHostResources>,
        accelerator_memory: Option<AcceleratorMemoryObservation>,
    ) -> HostedService {
        HostedService {
            service_name: service_name.to_owned(),
            host,
            accelerator_memory,
        }
    }

    /// Host figures observed at second `second`.
    fn host(second: i64, memory_bytes: u64, cpu_utilization: Option<f64>) -> ServiceHostResources {
        ServiceHostResources {
            observed_at: at(second),
            memory_bytes,
            cpu_utilization,
        }
    }

    /// Accelerator memory of `devices` as (device id, bytes), observed at second `second`.
    fn accelerator(second: i64, devices: &[(&str, u64)]) -> AcceleratorMemoryObservation {
        AcceleratorMemoryObservation {
            observed_at: Some(crate::metrics::timestamp(at(second))),
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
            observed_at: Some(crate::metrics::timestamp(at(0))),
            refresh: None,
            result: Some(accelerator_memory_observation::Result::Unavailable(
                AcceleratorMemoryUnavailable {
                    code: "WORKLOAD_PROCESS_IDENTITY_UNAVAILABLE".to_owned(),
                    message: "unavailable".to_owned(),
                },
            )),
        }
    }

    /// Hosted applications the test sets.
    struct FixedHosted(Vec<HostedApplication>);

    impl HostedApplicationResources for FixedHosted {
        fn hosted_application_resources(&self) -> Vec<HostedApplication> {
            self.0.clone()
        }
    }

    struct ChannelRecorder(mpsc::UnboundedSender<Vec<MetricRecord>>);

    impl MetricRecorder for ChannelRecorder {
        fn record(&self, records: Vec<MetricRecord>) {
            let _ = self.0.send(records);
        }
    }
}
