//! Builds the metric records of the accelerator memory a node attributes to each model deployment
//! it hosts.
//!
//! The node records them once per hardware collection, from the same attribution the deployment's
//! status reports, so history and the current value never diverge and change in steps as the node
//! refreshes its observations. An attribution that is unavailable or not applicable records
//! nothing, so history shows a gap rather than a held stale value.

use std::time::{Duration, SystemTime};

use dotcore_observability::{
    MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricRecord, MetricUnit,
    labels::{self, LabelSet},
};
use dotcore_proto::dotcore::deployment::v1::{
    AcceleratorMemoryObservation, accelerator_memory_observation,
};

use crate::collection_metrics::COLLECTION_REPORTING_DELAY;

/// Accelerator memory attributed to one model deployment on its hosting node, per device when the
/// node names one. The value is held from the latest attribution, which refreshes only at the
/// site's hardware refresh interval, so minimum and maximum over coarser buckets describe held
/// values rather than per-second measurements.
pub(crate) const DEPLOYMENT_ACCELERATOR_MEMORY_USED: &str = "deployment.accelerator.memory.used";

/// Writer name a node records deployment memory under, the `source` of its `node.observing`
/// window.
pub(crate) const DEPLOYMENT_MEMORY_SOURCE: &str = "deployment_memory";

/// Declarations of the deployment memory metrics hosting nodes record once per hardware
/// collection, every `refresh_interval`.
pub(crate) fn deployment_metric_descriptors(refresh_interval: Duration) -> Vec<MetricDescriptor> {
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
        sample_interval: Some(refresh_interval),
        reporting_delay: Some(COLLECTION_REPORTING_DELAY),
    }]
}

/// Current accelerator memory attribution of the model deployments one node hosts.
pub(crate) trait HostedAcceleratorMemory: Send + Sync {
    /// The attribution of each hosted deployment, by deployment id, as its status reports it.
    fn hosted_accelerator_memory(&self) -> Vec<(String, AcceleratorMemoryObservation)>;
}

/// One record per device row of every deployment SiteNode `node_id` hosts whose attribution in
/// `hosted` is available, stamped `at`.
pub(crate) fn deployment_memory_records(
    node_id: &str,
    hosted: &dyn HostedAcceleratorMemory,
    at: SystemTime,
) -> Vec<MetricRecord> {
    let mut records = vec![];
    for (deployment_id, memory) in hosted.hosted_accelerator_memory() {
        let Some(accelerator_memory_observation::Result::Available(usage)) = memory.result else {
            continue;
        };
        let deployment = LabelSet::new()
            .with(labels::DEPLOYMENT, deployment_id)
            .with(labels::NODE, node_id);
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

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use dotcore_proto::dotcore::deployment::v1::{
        AcceleratorMemoryDeviceUsage, AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet,
    };
    use dotcore_proto::google::protobuf::Empty;

    use super::*;

    #[test]
    fn each_device_row_of_every_available_deployment_records_at_the_given_time() {
        let hosted = FixedHosted(vec![
            (
                "deployment-a".to_owned(),
                available(&[("GPU-a", 100), ("GPU-b", 200)]),
            ),
            ("deployment-b".to_owned(), available(&[("", 300)])),
        ]);

        assert_eq!(
            deployment_memory_records("node-a", &hosted, at()),
            vec![
                record("deployment-a", Some("GPU-a"), 100.0),
                record("deployment-a", Some("GPU-b"), 200.0),
                record("deployment-b", None, 300.0),
            ]
        );
    }

    #[test]
    fn unavailable_and_not_applicable_attributions_record_nothing() {
        let hosted = FixedHosted(vec![
            ("available".to_owned(), available(&[("GPU-a", 1)])),
            ("unavailable".to_owned(), unavailable()),
            ("not-applicable".to_owned(), not_applicable()),
            (
                "without-a-result".to_owned(),
                AcceleratorMemoryObservation::default(),
            ),
        ]);

        assert_eq!(
            deployment_memory_records("node-a", &hosted, at()),
            vec![record("available", Some("GPU-a"), 1.0)]
        );
    }

    #[test]
    fn descriptors_hold_values_for_the_refresh_interval_plus_the_reporting_delay() {
        let [descriptor] = deployment_metric_descriptors(Duration::from_secs(7))
            .try_into()
            .expect("one descriptor");

        assert_eq!(descriptor.validate(), Ok(()));
        assert_eq!(descriptor.sample_interval, Some(Duration::from_secs(7)));
        assert_eq!(descriptor.gauge_lookback(), Some(Duration::from_secs(12)));
    }

    fn at() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// The record of `bytes` on `device` of `deployment` at the test time.
    fn record(deployment: &str, device: Option<&str>, bytes: f64) -> MetricRecord {
        let labels = LabelSet::new()
            .with("deployment", deployment)
            .with("node", "node-a");
        let labels = match device {
            Some(device) => labels.with("device", device),
            None => labels,
        };
        MetricRecord::gauge(DEPLOYMENT_ACCELERATOR_MEMORY_USED, labels, at(), bytes)
    }

    /// Usage of `devices` as (device id, bytes).
    fn available(devices: &[(&str, u64)]) -> AcceleratorMemoryObservation {
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

    /// Hosted deployments fixed by the test.
    struct FixedHosted(Vec<(String, AcceleratorMemoryObservation)>);

    impl HostedAcceleratorMemory for FixedHosted {
        fn hosted_accelerator_memory(&self) -> Vec<(String, AcceleratorMemoryObservation)> {
            self.0.clone()
        }
    }
}
