//! Node hardware observation values shared by node hardware collection, the site, and the simulated
//! runtime: one atomic snapshot of the host and its accelerators plus the processes using each
//! accelerator device, a light sample of memory, CPU, filesystem and accelerator signals taken
//! every second between snapshots, and the source that collects both.
//!
//! The values carry either complete rows or a typed unavailability, so a consumer never mistakes a
//! failed process observation for an idle device. For example, a vendor whose driver is missing
//! reports its process rows as unavailable rather than empty.

use dotcore_host_process::ProcessLifetime;
use dotcore_proto::dotcore::observation::v1::{
    AcceleratorVendor, HardwareSnapshot, ObservationUnavailable, accelerator_observation,
    host_observation,
};

/// One complete node hardware observation collected atomically.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeHardwareObservation {
    pub snapshot: HardwareSnapshot,
    pub accelerator_processes: AcceleratorProcessObservation,
}

/// Current per-vendor accelerator-process rows, or a typed component-level unavailability.
#[derive(Clone, Debug, PartialEq)]
pub enum AcceleratorProcessObservation {
    Observed(Vec<AcceleratorVendorProcessObservation>),

    Unavailable(ObservationUnavailable),
}

/// Current process observation for one registered accelerator vendor.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceleratorVendorProcessObservation {
    pub vendor: AcceleratorVendor,
    pub result: AcceleratorVendorProcessResult,
}

/// Complete process rows or typed unavailability for one accelerator vendor.
#[derive(Clone, Debug, PartialEq)]
pub enum AcceleratorVendorProcessResult {
    Observed(Vec<ObservedAcceleratorProcess>),

    Unavailable(ObservationUnavailable),
}

/// Current process memory observed on one canonical accelerator device.
/// Ancestors, nearest parent first, let consumers attribute a worker process to the workload
/// root that spawned it without reading the host's process table themselves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedAcceleratorProcess {
    pub device_id: String,
    pub process_id: u32,
    pub process_start_token: u64,
    pub ancestors: Vec<ProcessLifetime>,
    pub used_memory_bytes: Option<u64>,
}

/// Readings of the local host and its accelerators from one light pass, cheap enough to take every
/// second. A reading that could not be taken is absent, never zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HardwareSample {
    /// Host memory, CPU and filesystem readings.
    pub host: HostSample,

    /// Accelerators with a vendor device identity and their readings.
    pub accelerators: Vec<AcceleratorSample>,
}

impl HardwareSample {
    /// The readings of a full snapshot: its host memory and the memory of every accelerator with
    /// a device identity; unavailable parts, and signals a snapshot does not carry, read as
    /// absent.
    pub fn from_snapshot(snapshot: &HardwareSnapshot) -> Self {
        let memory = snapshot.host.as_ref().and_then(|host| match &host.result {
            Some(host_observation::Result::Facts(facts)) => Some(HostMemorySample {
                total_bytes: facts.total_memory_bytes,
                available_bytes: facts.available_memory_bytes,
                cached_bytes: facts.cached_memory_bytes,
            }),
            _ => None,
        });
        let accelerators = snapshot
            .accelerators
            .iter()
            .filter_map(|accelerator| match &accelerator.result {
                Some(accelerator_observation::Result::Facts(facts)) => Some(&facts.facts),
                _ => None,
            })
            .flatten()
            .filter(|device| !device.device_id.is_empty())
            .map(|device| AcceleratorSample {
                memory_total_bytes: device.total_memory_bytes,
                memory_used_bytes: device.used_memory_bytes,
                ..AcceleratorSample::new(device.device_id.clone())
            })
            .collect();
        Self {
            host: HostSample {
                memory,
                ..HostSample::default()
            },
            accelerators,
        }
    }
}

/// Readings of the local host from one light pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HostSample {
    /// Host memory; absent when the host could not be read.
    pub memory: Option<HostMemorySample>,

    /// Share of time, from 0 to 1, the host's CPUs spent outside idle since the previous sample,
    /// across all cores; absent for the first sample, which has nothing to compare with.
    pub cpu_utilization: Option<f64>,

    /// Space of every real filesystem whose statistics could be read.
    pub filesystems: Vec<FilesystemSample>,
}

/// Host memory in bytes: total, obtainable without evicting other applications' working memory,
/// and reclaimable file cache when the host reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMemorySample {
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub cached_bytes: Option<u64>,
}

/// Space of one real filesystem in bytes. Blocks reserved for the superuser count in the size but
/// not in the available space, so used plus available can be less than the size.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilesystemSample {
    /// Mount point path.
    pub mountpoint: String,

    /// Filesystem type, such as ext4 or apfs.
    pub fstype: String,

    pub size_bytes: u64,

    pub used_bytes: u64,

    /// Space an unprivileged writer can still use.
    pub available_bytes: u64,
}

/// Readings of one accelerator device; each is absent when the device does not support it or it
/// could not be read this time, independently of the others.
#[derive(Clone, Debug, PartialEq)]
pub struct AcceleratorSample {
    /// Vendor-native immutable device identity.
    pub device_id: String,

    /// Dedicated memory; absent for a device that allocates from host memory.
    pub memory_total_bytes: Option<u64>,

    /// Used dedicated memory, device-wide.
    pub memory_used_bytes: Option<u64>,

    /// Share of time, from 0 to 1, any kernel ran on the device over the driver's last sample
    /// period.
    pub utilization: Option<f64>,

    /// Die temperature in degrees Celsius.
    pub temperature_celsius: Option<f64>,

    /// Power draw in watts as the driver reports it.
    pub power_watts: Option<f64>,
}

impl AcceleratorSample {
    /// A sample of `device_id` with no reading.
    pub fn new(device_id: String) -> Self {
        Self {
            device_id,
            memory_total_bytes: None,
            memory_used_bytes: None,
            utilization: None,
            temperature_celsius: None,
            power_watts: None,
        }
    }
}

/// Collects one hardware observation on demand.
pub trait HardwareSnapshotSource {
    /// Returns one newly collected public hardware snapshot.
    fn collect_snapshot(&self) -> HardwareSnapshot;

    /// Returns one light sample, cheap enough to take every second. A node samples every second
    /// on its own schedule, so a sample never runs a full collection.
    fn sample(&self) -> HardwareSample;

    /// Returns one complete public and private node observation.
    fn collect_observation(&self) -> NodeHardwareObservation {
        NodeHardwareObservation {
            snapshot: self.collect_snapshot(),
            accelerator_processes: AcceleratorProcessObservation::Unavailable(
                ObservationUnavailable {
                    code: "PROCESS_OBSERVATION_UNAVAILABLE".to_owned(),
                    message: "Accelerator process observation is unavailable.".to_owned(),
                },
            ),
        }
    }
}

impl<S: HardwareSnapshotSource + ?Sized> HardwareSnapshotSource for Box<S> {
    fn collect_snapshot(&self) -> HardwareSnapshot {
        (**self).collect_snapshot()
    }

    fn collect_observation(&self) -> NodeHardwareObservation {
        (**self).collect_observation()
    }

    fn sample(&self) -> HardwareSample {
        (**self).sample()
    }
}
