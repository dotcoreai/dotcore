//! Declares the hardware metrics a node samples every second (host memory, CPU and filesystem
//! space, and per-device accelerator memory, utilization, temperature and power) and turns one
//! sample into metric records.
//!
//! Each record carries the sampling node's SiteNode id and, for accelerators, the vendor device
//! id, or for filesystems the mount point and type, timestamped with the time the node took the
//! sample. A reading a node could not take produces no record, so history shows a gap rather than
//! a made-up value.

use std::time::{Duration, SystemTime};

use dotcore_domain::hardware_observation::HardwareSample;
use dotcore_observability::{
    MetricDescriptor, MetricDimension, MetricKind, MetricProvenance, MetricRecord, MetricUnit,
    labels::{self, LabelSet},
};

/// Total physical memory of a node's host.
pub const HOST_MEMORY_TOTAL: &str = "node.host.memory.total";

/// Physical memory a workload can obtain on a node's host without evicting other applications'
/// working memory, including reclaimable file cache; unified accelerator allocations count as used.
pub const HOST_MEMORY_AVAILABLE: &str = "node.host.memory.available";

/// Reclaimable file cache of a node's host, already counted in its available memory.
pub const HOST_MEMORY_CACHED: &str = "node.host.memory.cached";

/// Dedicated memory of one accelerator device.
pub const ACCELERATOR_MEMORY_TOTAL: &str = "node.accelerator.memory.total";

/// Used dedicated memory of one accelerator device, device-wide.
pub const ACCELERATOR_MEMORY_USED: &str = "node.accelerator.memory.used";

/// Share of time, from 0 to 1, during which one or more kernels ran on one accelerator device
/// over the driver's last sample period (NVML GPU utilization); not the share of its compute units
/// in use.
pub const ACCELERATOR_UTILIZATION: &str = "node.accelerator.utilization";

/// Die temperature of one accelerator device in degrees Celsius, from its GPU sensor.
pub const ACCELERATOR_TEMPERATURE: &str = "node.accelerator.temperature";

/// Board or chip power of one accelerator device in watts, as its driver reports it: averaged over
/// one second on NVIDIA Ampere and newer, and excluding the CPU and memory on parts that share a
/// package with the CPU, such as GB10.
pub const ACCELERATOR_POWER: &str = "node.accelerator.power";

/// Share of time, from 0 to 1, a node host's CPUs spent outside idle since the previous sample,
/// across all cores, with no split by mode.
pub const HOST_CPU_UTILIZATION: &str = "node.host.cpu.utilization";

/// Used space of one real filesystem: its size minus its free blocks, reserved blocks excluded.
pub const FILESYSTEM_USED: &str = "node.filesystem.used";

/// Space of one real filesystem an unprivileged writer can still use; blocks reserved for the
/// superuser are not available, so used plus available can be less than the size.
pub const FILESYSTEM_AVAILABLE: &str = "node.filesystem.available";

/// Total size of one real filesystem, reserved blocks included.
pub const FILESYSTEM_SIZE: &str = "node.filesystem.size";

/// Seconds a node's hardware sampler could not sample in, because a sample overran, failed, or
/// read nothing.
pub const HARDWARE_SAMPLING_SKIPPED: &str = "node.hardware.sampling.skipped";

/// Writer name a node's hardware sampler records under, the `source` of its `node.observing`
/// window.
pub const HARDWARE_SOURCE: &str = "hardware";

/// Interval at which every node samples its hardware.
pub const HARDWARE_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

/// Declarations of the hardware metrics, sampled every [`HARDWARE_SAMPLE_INTERVAL`], and of the
/// seconds the sampler skipped.
pub fn hardware_metric_descriptors() -> Vec<MetricDescriptor> {
    let gauge = |name: &str, unit, dimensions: &[&str]| MetricDescriptor {
        name: name.to_owned(),
        kind: MetricKind::Gauge,
        unit,
        dimensions: dimensions
            .iter()
            .map(|dimension| MetricDimension::required(*dimension))
            .collect(),
        provenance: MetricProvenance::Node,
        sample_interval: Some(HARDWARE_SAMPLE_INTERVAL),
        reporting_delay: None,
    };
    vec![
        gauge(HOST_MEMORY_TOTAL, MetricUnit::Bytes, NODE),
        gauge(HOST_MEMORY_AVAILABLE, MetricUnit::Bytes, NODE),
        gauge(HOST_MEMORY_CACHED, MetricUnit::Bytes, NODE),
        gauge(HOST_CPU_UTILIZATION, MetricUnit::Ratio, NODE),
        gauge(FILESYSTEM_USED, MetricUnit::Bytes, FILESYSTEM),
        gauge(FILESYSTEM_AVAILABLE, MetricUnit::Bytes, FILESYSTEM),
        gauge(FILESYSTEM_SIZE, MetricUnit::Bytes, FILESYSTEM),
        gauge(ACCELERATOR_MEMORY_TOTAL, MetricUnit::Bytes, DEVICE),
        gauge(ACCELERATOR_MEMORY_USED, MetricUnit::Bytes, DEVICE),
        gauge(ACCELERATOR_UTILIZATION, MetricUnit::Ratio, DEVICE),
        gauge(ACCELERATOR_TEMPERATURE, MetricUnit::DegreesCelsius, DEVICE),
        gauge(ACCELERATOR_POWER, MetricUnit::Watts, DEVICE),
        MetricDescriptor {
            name: HARDWARE_SAMPLING_SKIPPED.to_owned(),
            kind: MetricKind::Counter,
            unit: MetricUnit::Seconds,
            dimensions: vec![MetricDimension::required(labels::NODE)],
            provenance: MetricProvenance::Node,
            sample_interval: None,
            reporting_delay: None,
        },
    ]
}

/// Metric records of one sample SiteNode `node_id` took at `observed_at`.
pub(crate) fn hardware_sample_records(
    node_id: &str,
    sample: &HardwareSample,
    observed_at: SystemTime,
) -> Vec<MetricRecord> {
    let node = LabelSet::new().with(labels::NODE, node_id);
    let mut records = vec![];
    let mut gauge = |metric: &str, labels: &LabelSet, value: Option<f64>| {
        if let Some(value) = value {
            records.push(MetricRecord::gauge(
                metric,
                labels.clone(),
                observed_at,
                value,
            ));
        }
    };
    let host = &sample.host;
    if let Some(memory) = &host.memory {
        gauge(HOST_MEMORY_TOTAL, &node, Some(memory.total_bytes as f64));
        gauge(
            HOST_MEMORY_AVAILABLE,
            &node,
            Some(memory.available_bytes as f64),
        );
        gauge(
            HOST_MEMORY_CACHED,
            &node,
            memory.cached_bytes.map(|bytes| bytes as f64),
        );
    }
    gauge(HOST_CPU_UTILIZATION, &node, host.cpu_utilization);
    for filesystem in &host.filesystems {
        let labels = node
            .clone()
            .with(MOUNTPOINT, filesystem.mountpoint.clone())
            .with(FSTYPE, filesystem.fstype.clone());
        gauge(FILESYSTEM_USED, &labels, Some(filesystem.used_bytes as f64));
        gauge(
            FILESYSTEM_AVAILABLE,
            &labels,
            Some(filesystem.available_bytes as f64),
        );
        gauge(FILESYSTEM_SIZE, &labels, Some(filesystem.size_bytes as f64));
    }
    for device in &sample.accelerators {
        let labels = node.clone().with(labels::DEVICE, device.device_id.clone());
        let bytes = |bytes: Option<u64>| bytes.map(|bytes| bytes as f64);
        gauge(
            ACCELERATOR_MEMORY_TOTAL,
            &labels,
            bytes(device.memory_total_bytes),
        );
        gauge(
            ACCELERATOR_MEMORY_USED,
            &labels,
            bytes(device.memory_used_bytes),
        );
        gauge(ACCELERATOR_UTILIZATION, &labels, device.utilization);
        gauge(ACCELERATOR_TEMPERATURE, &labels, device.temperature_celsius);
        gauge(ACCELERATOR_POWER, &labels, device.power_watts);
    }
    records
}

/// The record of `seconds` SiteNode `node_id`'s hardware sampler skipped, noticed at
/// `observed_at`.
pub(crate) fn hardware_sampling_skipped_record(
    node_id: &str,
    seconds: u64,
    observed_at: SystemTime,
) -> MetricRecord {
    MetricRecord::counter(
        HARDWARE_SAMPLING_SKIPPED,
        LabelSet::new().with(labels::NODE, node_id),
        observed_at,
        seconds as f64,
    )
}

/// Dimension naming the mount point of a filesystem.
const MOUNTPOINT: &str = "mountpoint";

/// Dimension naming the type of a filesystem.
const FSTYPE: &str = "fstype";

/// Dimensions of a node host's metrics.
const NODE: &[&str] = &[labels::NODE];

/// Dimensions of a filesystem's metrics, named as Prometheus and OpenTelemetry name them.
const FILESYSTEM: &[&str] = &[labels::NODE, MOUNTPOINT, FSTYPE];

/// Dimensions of an accelerator device's metrics.
const DEVICE: &[&str] = &[labels::NODE, labels::DEVICE];

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use dotcore_domain::hardware_observation::{
        AcceleratorSample, FilesystemSample, HostMemorySample, HostSample,
    };

    use super::*;

    #[test]
    fn a_sample_becomes_host_filesystem_and_device_records() {
        let at = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let records = hardware_sample_records("gpu-host", &full_sample(), at);

        let node_labels = LabelSet::new().with("node", "gpu-host");
        let root_labels = node_labels
            .clone()
            .with("mountpoint", "/")
            .with("fstype", "ext4");
        let device_labels = node_labels.clone().with("device", "GPU-a");
        let gb10_labels = node_labels.clone().with("device", "GPU-gb10");
        assert_eq!(
            records,
            vec![
                MetricRecord::gauge(HOST_MEMORY_TOTAL, node_labels.clone(), at, 64.0),
                MetricRecord::gauge(HOST_MEMORY_AVAILABLE, node_labels.clone(), at, 40.0),
                MetricRecord::gauge(HOST_MEMORY_CACHED, node_labels.clone(), at, 16.0),
                MetricRecord::gauge(HOST_CPU_UTILIZATION, node_labels, at, 0.25),
                MetricRecord::gauge(FILESYSTEM_USED, root_labels.clone(), at, 600.0),
                MetricRecord::gauge(FILESYSTEM_AVAILABLE, root_labels.clone(), at, 350.0),
                MetricRecord::gauge(FILESYSTEM_SIZE, root_labels, at, 1_000.0),
                MetricRecord::gauge(ACCELERATOR_MEMORY_TOTAL, device_labels.clone(), at, 32.0),
                MetricRecord::gauge(ACCELERATOR_MEMORY_USED, device_labels.clone(), at, 12.0),
                MetricRecord::gauge(ACCELERATOR_UTILIZATION, device_labels.clone(), at, 0.875),
                MetricRecord::gauge(ACCELERATOR_TEMPERATURE, device_labels.clone(), at, 61.0),
                MetricRecord::gauge(ACCELERATOR_POWER, device_labels, at, 412.5),
                MetricRecord::gauge(ACCELERATOR_UTILIZATION, gb10_labels.clone(), at, 0.1),
                MetricRecord::gauge(ACCELERATOR_TEMPERATURE, gb10_labels, at, 45.0),
            ]
        );
    }

    #[test]
    fn an_empty_sample_produces_no_records() {
        assert_eq!(
            hardware_sample_records("gpu-host", &HardwareSample::default(), UNIX_EPOCH),
            vec![]
        );
    }

    #[test]
    fn every_record_fits_its_declaration() {
        let descriptors = hardware_metric_descriptors();

        for record in hardware_sample_records("gpu-host", &full_sample(), UNIX_EPOCH) {
            let descriptor = descriptors
                .iter()
                .find(|descriptor| descriptor.name == record.metric)
                .expect("declared metric");
            assert_eq!(descriptor.validate(), Ok(()));
            assert_eq!(descriptor.validate_labels(&record.labels), Ok(()));
            assert_eq!(descriptor.sample_interval, Some(Duration::from_secs(1)));
        }
    }

    #[test]
    fn signal_metrics_declare_their_units() {
        let descriptors = hardware_metric_descriptors();
        let unit = |name: &str| {
            descriptors
                .iter()
                .find(|descriptor| descriptor.name == name)
                .map(|descriptor| (descriptor.kind, descriptor.unit))
        };

        assert_eq!(
            [
                HOST_CPU_UTILIZATION,
                FILESYSTEM_USED,
                FILESYSTEM_AVAILABLE,
                FILESYSTEM_SIZE,
                ACCELERATOR_UTILIZATION,
                ACCELERATOR_TEMPERATURE,
                ACCELERATOR_POWER,
            ]
            .map(unit),
            [
                Some((MetricKind::Gauge, MetricUnit::Ratio)),
                Some((MetricKind::Gauge, MetricUnit::Bytes)),
                Some((MetricKind::Gauge, MetricUnit::Bytes)),
                Some((MetricKind::Gauge, MetricUnit::Bytes)),
                Some((MetricKind::Gauge, MetricUnit::Ratio)),
                Some((MetricKind::Gauge, MetricUnit::DegreesCelsius)),
                Some((MetricKind::Gauge, MetricUnit::Watts)),
            ]
        );
    }

    /// A host with cached memory, CPU utilization and one filesystem, and two devices: one
    /// dedicated with every signal, one allocating from host memory without power.
    fn full_sample() -> HardwareSample {
        HardwareSample {
            host: HostSample {
                memory: Some(HostMemorySample {
                    total_bytes: 64,
                    available_bytes: 40,
                    cached_bytes: Some(16),
                }),
                cpu_utilization: Some(0.25),
                filesystems: vec![FilesystemSample {
                    mountpoint: "/".to_owned(),
                    fstype: "ext4".to_owned(),
                    size_bytes: 1_000,
                    used_bytes: 600,
                    available_bytes: 350,
                }],
            },
            accelerators: vec![
                AcceleratorSample {
                    memory_total_bytes: Some(32),
                    memory_used_bytes: Some(12),
                    utilization: Some(0.875),
                    temperature_celsius: Some(61.0),
                    power_watts: Some(412.5),
                    ..AcceleratorSample::new("GPU-a".to_owned())
                },
                AcceleratorSample {
                    utilization: Some(0.1),
                    temperature_celsius: Some(45.0),
                    ..AcceleratorSample::new("GPU-gb10".to_owned())
                },
            ],
        }
    }
}
