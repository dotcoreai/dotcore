//! Builds the metric records of the host CPU, host memory and accelerator memory a node
//! attributes to each service of the application deployments it hosts.
//!
//! The node records them once per hardware collection, from what its application status
//! attributes, so history and the current value never diverge and change in steps as the node
//! refreshes its observations. A figure that is unavailable records nothing, so history shows a
//! gap rather than a held stale value.

use std::time::{Duration, SystemTime};

use dotcore_observability::{
    MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricRecord, MetricUnit,
    labels::{self, LabelSet},
};
use dotcore_proto::dotcore::deployment::v1::{
    AcceleratorMemoryObservation, accelerator_memory_observation,
};

use super::resources::ServiceHostResources;
use crate::collection_metrics::COLLECTION_REPORTING_DELAY;

/// Share, from 0 to 1, of the hosting machine's logical CPUs one application service kept busy
/// between the node's two latest readings of it.
pub(crate) const APPLICATION_HOST_CPU_UTILIZATION: &str = "application.host.cpu.utilization";

/// Host memory one application service's processes hold, not counting reclaimable file cache.
pub(crate) const APPLICATION_HOST_MEMORY_USED: &str = "application.host.memory.used";

/// Accelerator memory attributed to one application service on its hosting node, per device when
/// the node names one.
pub(crate) const APPLICATION_ACCELERATOR_MEMORY_USED: &str = "application.accelerator.memory.used";

/// Writer name a node records application resources under, the `source` of its
/// `node.observing` window.
pub(crate) const APPLICATION_RESOURCES_SOURCE: &str = "application_resources";

/// Declarations of the application resource metrics hosting nodes record once per hardware
/// collection, every `refresh_interval`. Every value is held from the latest collection, so
/// minimum and maximum over coarser buckets describe held values rather than per-second
/// measurements.
pub(crate) fn application_metric_descriptors(refresh_interval: Duration) -> Vec<MetricDescriptor> {
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
            sample_interval: Some(refresh_interval),
            reporting_delay: Some(COLLECTION_REPORTING_DELAY),
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

/// The records of every service SiteNode `node_id` hosts with figures in `hosted`, stamped `at`:
/// its CPU share and host memory, and one accelerator memory record per available device row.
pub(crate) fn application_resource_records(
    node_id: &str,
    hosted: &dyn HostedApplicationResources,
    at: SystemTime,
) -> Vec<MetricRecord> {
    let mut records = vec![];
    for application in hosted.hosted_application_resources() {
        for service in application.services {
            let labels = LabelSet::new()
                .with(labels::DEPLOYMENT, application.deployment_id.clone())
                .with(labels::SERVICE, service.service_name)
                .with(labels::NODE, node_id);
            if let Some(host) = service.host {
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
                accelerator_records(&labels, memory, at, &mut records);
            }
        }
    }
    records
}

/// Appends one record per device row of `memory` when it is available.
fn accelerator_records(
    service: &LabelSet,
    memory: AcceleratorMemoryObservation,
    at: SystemTime,
    records: &mut Vec<MetricRecord>,
) {
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

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use dotcore_proto::dotcore::deployment::v1::{
        AcceleratorMemoryDeviceUsage, AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet,
    };

    use super::*;

    #[test]
    fn each_services_cpu_memory_and_device_rows_record_at_the_given_time() {
        let hosted = FixedHosted(vec![application(
            "app-1",
            vec![
                service("cache", Some(host(2048, None)), None),
                service(
                    "web",
                    Some(host(4096, Some(0.25))),
                    Some(accelerator(&[("GPU-a", 100), ("", 300)])),
                ),
            ],
        )]);

        assert_eq!(
            application_resource_records("node-a", &hosted, at()),
            vec![
                record(APPLICATION_HOST_MEMORY_USED, "cache", None, 2048.0),
                record(APPLICATION_HOST_CPU_UTILIZATION, "web", None, 0.25),
                record(APPLICATION_HOST_MEMORY_USED, "web", None, 4096.0),
                record(
                    APPLICATION_ACCELERATOR_MEMORY_USED,
                    "web",
                    Some("GPU-a"),
                    100.0
                ),
                record(APPLICATION_ACCELERATOR_MEMORY_USED, "web", None, 300.0),
            ]
        );
    }

    #[test]
    fn services_without_figures_record_nothing() {
        let hosted = FixedHosted(vec![application(
            "app-1",
            vec![
                service("without-figures", None, Some(unavailable())),
                service("with-figures", Some(host(1, Some(0.5))), None),
            ],
        )]);

        assert_eq!(
            application_resource_records("node-a", &hosted, at()),
            vec![
                record(APPLICATION_HOST_CPU_UTILIZATION, "with-figures", None, 0.5),
                record(APPLICATION_HOST_MEMORY_USED, "with-figures", None, 1.0),
            ]
        );
    }

    #[test]
    fn descriptors_hold_values_for_the_refresh_interval_plus_the_reporting_delay() {
        for descriptor in application_metric_descriptors(Duration::from_secs(7)) {
            assert_eq!(descriptor.validate(), Ok(()), "{}", descriptor.name);
            assert_eq!(descriptor.sample_interval, Some(Duration::from_secs(7)));
            assert_eq!(descriptor.gauge_lookback(), Some(Duration::from_secs(12)));
        }
    }

    /// The record of `value` for `service` of `app-1`, on `device` when given, at the test time.
    fn record(metric: &str, service: &str, device: Option<&str>, value: f64) -> MetricRecord {
        let labels = LabelSet::new()
            .with("deployment", "app-1")
            .with("service", service)
            .with("node", "node-a");
        let labels = match device {
            Some(device) => labels.with("device", device),
            None => labels,
        };
        MetricRecord::gauge(metric, labels, at(), value)
    }

    fn at() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
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

    fn host(memory_bytes: u64, cpu_utilization: Option<f64>) -> ServiceHostResources {
        ServiceHostResources {
            observed_at: at(),
            memory_bytes,
            cpu_utilization,
        }
    }

    /// Accelerator memory of `devices` as (device id, bytes).
    fn accelerator(devices: &[(&str, u64)]) -> AcceleratorMemoryObservation {
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
}
