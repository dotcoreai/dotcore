//! Collects a snapshot of the local host and its accelerators for the node.
//!
//! Host facts are read from the operating system and per-device facts from each injected
//! vendor adapter (for example the NVIDIA NVML, AMD, and Apple Metal adapters), and combined into
//! one hardware snapshot. Collection is read-only and never touches workload state or persistent
//! files; it supplies the hardware view shown on the Machines page. A separate light sample reads
//! host memory, CPU and filesystem space and per-device accelerator signals, cheap enough to take
//! every second.

use std::{
    collections::BTreeMap,
    fs,
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use dotcore_domain::{
    clock::Clock,
    hardware_observation::{
        AcceleratorProcessObservation, AcceleratorSample, AcceleratorVendorProcessObservation,
        AcceleratorVendorProcessResult, HardwareSample, HardwareSnapshotSource, HostMemorySample,
        HostSample, NodeHardwareObservation, ObservedAcceleratorProcess,
    },
};
use dotcore_host_process::{HostProcessTable, ProcessLifetime, ProcessTable, process_ancestors};
use dotcore_proto::{
    dotcore::observation::v1::{
        AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
        AcceleratorObservation, AcceleratorSignal, AcceleratorVendor, HardwareSnapshot, HostFacts,
        HostObservation, ObservationUnavailable, accelerator_observation, host_observation,
    },
    google::protobuf::Timestamp,
};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};
use tracing::{Span, debug, debug_span, field, warn};

use crate::{
    cpu::CpuUtilization,
    filesystem::Filesystems,
    host_memory::HostMemory,
    nvidia::{NvidiaObservationSource, SystemNvidiaObservationSource},
};

/// Reads required host facts from the local operating system.
pub trait HostFactsAdapter {
    /// Returns local host facts or a stable unavailability reason.
    fn collect_host_facts(&self) -> Result<HostFacts, UnavailableReason>;

    /// Returns the current host readings. Adapters without a cheaper path read memory from full
    /// host facts and report no CPU or filesystem readings.
    fn sample(&self) -> HostSample {
        let memory = self
            .collect_host_facts()
            .ok()
            .map(|facts| HostMemorySample {
                total_bytes: facts.total_memory_bytes,
                available_bytes: facts.available_memory_bytes,
                cached_bytes: facts.cached_memory_bytes,
            });
        HostSample {
            memory,
            ..HostSample::default()
        }
    }
}

/// Discovers one production-supported accelerator vendor on the local host.
pub trait AcceleratorVendorDiscoverer: Send + Sync {
    /// Returns the accelerator vendor discovered by this adapter.
    fn vendor(&self) -> AcceleratorVendor;

    /// Discovers the complete fixed device set once for this backend process.
    fn discover(&self) -> AcceleratorDiscovery;
}

/// One complete vendor-discovery outcome retained until backend restart.
pub enum AcceleratorDiscovery {
    Detected(Box<dyn AcceleratorVendorPoller>),

    Absent,

    Unavailable(UnavailableReason),
}

/// Polls current facts and process rows for one fixed discovered device set.
pub trait AcceleratorVendorPoller: Send {
    /// Returns one current observation without changing the discovered device set.
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason>;

    /// Returns a reader of the devices' per-second signals, usable while a poll runs; `None` for
    /// a vendor without a collector, such as Apple Silicon GPUs, whose signals are unsupported.
    fn sampler(&self) -> Option<Arc<dyn AcceleratorSampler>> {
        None
    }
}

/// Reads the current dedicated memory, utilization, temperature and power of one fixed discovered
/// device set, without facts or process rows, independently of any full poll.
pub trait AcceleratorSampler: Send + Sync {
    /// Returns the readings of every device that could be read. A device or one of its signals
    /// failing leaves out only that device or signal.
    fn sample(&self) -> Vec<AcceleratorSample>;
}

/// Reads per-device accelerator facts for one vendor from the local host.
pub trait AcceleratorVendorAdapter: Send + Sync {
    /// Returns the accelerator vendor probed by this adapter.
    fn vendor(&self) -> AcceleratorVendor;

    /// Returns per-device facts or a typed stable reason with optional diagnostics.
    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable>;
}

/// Accelerator collection failure with a stable public reason and optional internal source.
#[derive(Debug, thiserror::Error)]
#[error("{reason:?}")]
pub struct AcceleratorUnavailable {
    reason: UnavailableReason,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl AcceleratorUnavailable {
    /// Creates a failure with no internal diagnostic source.
    pub fn stable(reason: UnavailableReason) -> Self {
        Self {
            reason,
            source: None,
        }
    }

    /// Creates a failure that retains an internal diagnostic source.
    pub fn with_source<E>(reason: UnavailableReason, source: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self {
            reason,
            source: Some(Box::new(source)),
        }
    }

    /// Returns the stable reason projected into observations and telemetry.
    pub fn reason(&self) -> UnavailableReason {
        self.reason
    }

    /// Splits the stable reason from the internal source at an owning boundary.
    pub fn into_parts(
        self,
    ) -> (
        UnavailableReason,
        Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) {
        (self.reason, self.source)
    }
}

/// Facts and process rows collected from one accelerator vendor in one pass.
pub struct AcceleratorVendorObservation {
    pub facts: Vec<AcceleratorFacts>,
    pub processes: Vec<ObservedAcceleratorProcess>,
}

/// Combines independent host and registered accelerator observers into fresh snapshots, stamped
/// with the backend clock's wall time.
pub struct HardwareSnapshotCollector<H> {
    host: H,
    accelerators: Vec<AcceleratorRegistration>,
    clock: Arc<dyn Clock>,
}

/// Host-local hardware collector that uses sysinfo and the vendor accelerator adapters.
pub type SystemHardwareSnapshotCollector = HardwareSnapshotCollector<SystemHostFactsAdapter>;

/// Reads host facts through sysinfo and filesystem statistics, keeping the previous CPU reading
/// and the filesystem list between samples.
pub struct SystemHostFactsAdapter {
    cpu: CpuUtilization,
    filesystems: Filesystems,
}

/// Reads NVIDIA accelerator facts through NVML.
pub struct SystemNvidiaAcceleratorAdapter {
    open_inventory: Box<
        dyn Fn() -> Result<Box<dyn NvidiaObservationSource>, AcceleratorUnavailable> + Send + Sync,
    >,
    processes: Arc<dyn ProcessTable>,
}

/// Reads AMD accelerator facts through sysfs.
pub struct SystemAmdAcceleratorAdapter {
    root: PathBuf,
}

impl<H> HardwareSnapshotCollector<H> {
    /// Creates a collector with explicit host and accelerator discovery adapters, stamping
    /// snapshots with `clock`.
    pub fn new(
        host: H,
        accelerators: Vec<Box<dyn AcceleratorVendorDiscoverer>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            host,
            accelerators: accelerators
                .into_iter()
                .map(AcceleratorRegistration::new)
                .collect(),
            clock,
        }
    }
}

impl SystemHardwareSnapshotCollector {
    /// Creates a collector for the current host.
    pub fn for_current_host(
        accelerators: Vec<Box<dyn AcceleratorVendorDiscoverer>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self::new(SystemHostFactsAdapter::new(), accelerators, clock)
    }

    /// Creates a collector for the current host with the accelerator vendors its platform supports.
    pub fn with_platform_accelerators(clock: Arc<dyn Clock>) -> Self {
        Self::for_current_host(platform_accelerator_discoverers(), clock)
    }
}

/// Returns the accelerator vendors observable on this platform: NVIDIA on Linux, Apple on macOS.
pub fn platform_accelerator_discoverers() -> Vec<Box<dyn AcceleratorVendorDiscoverer>> {
    #[cfg(target_os = "linux")]
    let discoverers: Vec<Box<dyn AcceleratorVendorDiscoverer>> =
        vec![Box::new(SystemNvidiaAcceleratorAdapter::for_current_host())];
    #[cfg(target_os = "macos")]
    let discoverers: Vec<Box<dyn AcceleratorVendorDiscoverer>> = vec![Box::new(
        crate::apple::SystemAppleAcceleratorAdapter::for_current_host(),
    )];
    discoverers
}

impl<H> HardwareSnapshotSource for HardwareSnapshotCollector<H>
where
    H: HostFactsAdapter,
{
    fn collect_snapshot(&self) -> HardwareSnapshot {
        self.collect_observation().snapshot
    }

    fn collect_observation(&self) -> NodeHardwareObservation {
        let span = debug_span!(
            "hardware_observation",
            operation = "hardware_observation",
            component = field::Empty,
            outcome = field::Empty,
            error_kind = field::Empty,
        );
        let host_result = self.host.collect_host_facts();
        record_observation(&span, "host", &host_result);
        let mut accelerators = Vec::with_capacity(self.accelerators.len());
        let mut processes = Vec::with_capacity(self.accelerators.len());
        for registration in &self.accelerators {
            let vendor = registration.vendor;
            let result = registration.collect();
            record_observation(&span, vendor_component(vendor), &result);
            match result {
                Ok(observation) => {
                    processes.push(AcceleratorVendorProcessObservation {
                        vendor,
                        result: AcceleratorVendorProcessResult::Observed(observation.processes),
                    });
                    accelerators.push(accelerator_observation(vendor, Ok(observation.facts)));
                }
                Err(reason) => {
                    let unavailable = unavailable(reason, vendor_source(vendor));
                    processes.push(AcceleratorVendorProcessObservation {
                        vendor,
                        result: AcceleratorVendorProcessResult::Unavailable(unavailable),
                    });
                    accelerators.push(accelerator_observation(
                        vendor,
                        Err(AcceleratorUnavailable::stable(reason)),
                    ));
                }
            }
        }
        span.record(
            "outcome",
            if host_result.is_ok() {
                "success"
            } else {
                "unavailable"
            },
        );
        let accelerator_processes = AcceleratorProcessObservation::Observed(processes);
        NodeHardwareObservation {
            snapshot: HardwareSnapshot {
                observed_at: Some(timestamp_from(self.clock.wall_now())),
                host: Some(host_observation(host_result)),
                accelerators,
            },
            accelerator_processes,
        }
    }

    /// Reads the host and the signals of already discovered accelerators only; a vendor not
    /// discovered yet, or failing, contributes nothing. Processes, the filesystem list, host name
    /// and operating system stay on the snapshot cadence.
    fn sample(&self) -> HardwareSample {
        let accelerators = self
            .accelerators
            .iter()
            .flat_map(AcceleratorRegistration::sample)
            .collect();
        HardwareSample {
            host: self.host.sample(),
            accelerators,
        }
    }
}

/// Stable causes for unavailable hardware observations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnavailableReason {
    Unsupported,

    PermissionDenied,

    MissingLibrary,

    NoDevice,

    CollectionFailed,
}

/// Converts a system time into a protobuf timestamp, saturating times outside its range.
pub fn timestamp_from(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => Timestamp {
            seconds: duration.as_secs().try_into().unwrap_or(i64::MAX),
            nanos: duration
                .subsec_nanos()
                .try_into()
                .expect("nanoseconds fit i32"),
        },
        Err(error) => {
            let duration = error.duration();
            Timestamp {
                seconds: -i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                nanos: -i32::try_from(duration.subsec_nanos()).expect("nanoseconds fit i32"),
            }
        }
    }
}

struct AcceleratorRegistration {
    vendor: AcceleratorVendor,
    state: Mutex<AcceleratorDiscoveryState>,

    /// Signal reader of the discovered devices, kept apart from `state` so a sample never waits
    /// for a poll holding it.
    sampler: OnceLock<Arc<dyn AcceleratorSampler>>,
}

impl AcceleratorRegistration {
    fn new(discoverer: Box<dyn AcceleratorVendorDiscoverer>) -> Self {
        Self {
            vendor: discoverer.vendor(),
            state: Mutex::new(AcceleratorDiscoveryState::Pending(Some(discoverer))),
            sampler: OnceLock::new(),
        }
    }

    fn collect(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let AcceleratorDiscoveryState::Pending(discoverer) = &mut *state {
            let discoverer = discoverer
                .take()
                .expect("pending accelerator discovery owns one discoverer");
            *state = AcceleratorDiscoveryState::Unavailable(UnavailableReason::CollectionFailed);
            let outcome = catch_unwind(AssertUnwindSafe(|| discoverer.discover()));
            if let Ok(discovery) = outcome {
                *state = discovery.into();
            }
            if let AcceleratorDiscoveryState::Detected(poller) = &*state
                && let Ok(Some(sampler)) = catch_unwind(AssertUnwindSafe(|| poller.sampler()))
            {
                let _ = self.sampler.set(sampler);
            }
        }
        match &*state {
            AcceleratorDiscoveryState::Pending(_) => {
                unreachable!("accelerator discovery transitions before collection")
            }
            AcceleratorDiscoveryState::Detected(poller) => poller.poll(),
            AcceleratorDiscoveryState::Absent => Ok(AcceleratorVendorObservation {
                facts: Vec::new(),
                processes: Vec::new(),
            }),
            AcceleratorDiscoveryState::Unavailable(reason) => Err(*reason),
        }
    }

    /// Samples the discovered device set without waiting for a poll; empty before discovery and
    /// when the sampler panics.
    fn sample(&self) -> Vec<AcceleratorSample> {
        let Some(sampler) = self.sampler.get() else {
            return Vec::new();
        };
        catch_unwind(AssertUnwindSafe(|| sampler.sample())).unwrap_or_default()
    }
}

enum AcceleratorDiscoveryState {
    Pending(Option<Box<dyn AcceleratorVendorDiscoverer>>),

    Detected(Box<dyn AcceleratorVendorPoller>),

    Absent,

    Unavailable(UnavailableReason),
}

impl From<AcceleratorDiscovery> for AcceleratorDiscoveryState {
    fn from(discovery: AcceleratorDiscovery) -> Self {
        match discovery {
            AcceleratorDiscovery::Detected(poller) => Self::Detected(poller),
            AcceleratorDiscovery::Absent => Self::Absent,
            AcceleratorDiscovery::Unavailable(reason) => Self::Unavailable(reason),
        }
    }
}

impl SystemNvidiaAcceleratorAdapter {
    /// Creates an adapter that opens the process-wide NVML handle on collection.
    pub fn for_current_host() -> Self {
        Self {
            open_inventory: Box::new(|| {
                let source =
                    SystemNvidiaObservationSource::open().map_err(AcceleratorUnavailable::from)?;
                Ok(Box::new(source))
            }),
            processes: Arc::new(HostProcessTable),
        }
    }

    fn discover_nvidia(&self) -> Result<Option<NvidiaAcceleratorPoller>, UnavailableReason> {
        let inventory = (self.open_inventory)().map_err(|error| error.reason())?;
        let device_count = inventory
            .device_count()
            .map_err(|error| unavailable_reason_for_nvml_kind(error.kind()))?;
        if device_count == 0 {
            return Ok(None);
        }
        let devices = (0..device_count)
            .map(|index| {
                let uuid = inventory.device_uuid(index)?;
                Ok(NvidiaDevice {
                    index,
                    supported_signals: probe_supported_signals(inventory.as_ref(), &uuid),
                    uuid,
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error: dotcore_nvml::Unavailable| {
                unavailable_reason_for_nvml_kind(error.kind())
            })?;
        Ok(Some(NvidiaAcceleratorPoller {
            inventory: Arc::from(inventory),
            devices,
            processes: Arc::clone(&self.processes),
        }))
    }
}

impl SystemAmdAcceleratorAdapter {
    /// Creates an adapter that reads the live host sysfs and proc trees.
    pub fn for_current_host() -> Self {
        Self {
            root: PathBuf::from("/"),
        }
    }

    /// Creates an adapter rooted at one sysfs/proc tree for test fixtures.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Returns the `amdgpu` module version, falling back to the kernel release.
    fn driver_version(&self) -> Result<String, UnavailableReason> {
        // The amdgpu module does not publish a version on most hosts; the kernel
        // release is the stable fallback for driver_or_runtime_version.
        if let Some(version) = read_trimmed(&self.root.join("sys/module/amdgpu/version"))
            .ok()
            .filter(|version| !version.is_empty())
        {
            return Ok(version);
        }
        read_trimmed(&self.root.join("proc/sys/kernel/osrelease"))
            .map_err(|error| unavailable_reason_for_io_error(error.kind()))
    }

    /// Samples the dedicated memory of every AMD card exposing a `unique_id`, the identity its
    /// facts carry; a card without one has no stable identity and is skipped.
    pub fn sample(&self) -> Result<Vec<AcceleratorSample>, UnavailableReason> {
        let mut samples: Vec<_> = self
            .cards()?
            .into_iter()
            .filter(|card| !card.unique_id.is_empty())
            .map(|card| AcceleratorSample {
                memory_total_bytes: card.vram_total(),
                memory_used_bytes: read_u64(&card.device_root.join("mem_info_vram_used"))
                    .ok()
                    .flatten(),
                ..AcceleratorSample::new(card.unique_id)
            })
            .collect();
        samples.sort_by(|a, b| a.device_id.cmp(&b.device_id));
        Ok(samples)
    }

    /// Every AMD DRM card of the sysfs tree.
    fn cards(&self) -> Result<Vec<AmdCard>, UnavailableReason> {
        let drm_root = self.root.join("sys/class/drm");
        let mut cards = Vec::new();
        for entry in fs::read_dir(&drm_root)
            .map_err(|error| unavailable_reason_for_io_error(error.kind()))?
        {
            let entry = entry.map_err(|error| unavailable_reason_for_io_error(error.kind()))?;
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("card"))
            {
                continue;
            }
            let device_root = entry.path().join("device");
            // Real DRM cards own a `device` subdirectory; connectors hold a
            // symlink to the parent card's PCI device and must be skipped.
            let Ok(device_meta) = fs::symlink_metadata(&device_root) else {
                continue;
            };
            if !device_meta.is_dir() {
                continue;
            }
            let Some(vendor_id) = pci_id(&device_root.join("vendor"))? else {
                continue;
            };
            if vendor_id != AMD_PCI_VENDOR_ID {
                continue;
            }
            let Some(device_id) = pci_id(&device_root.join("device"))? else {
                continue;
            };
            cards.push(AmdCard {
                model: format!("{vendor_id}:{device_id}"),
                unique_id: read_trimmed(&device_root.join("unique_id")).unwrap_or_default(),
                device_root,
            });
        }
        Ok(cards)
    }
}

impl SystemHostFactsAdapter {
    /// Creates an adapter for the current host; its first sample reports no CPU utilization.
    pub fn new() -> Self {
        Self {
            cpu: CpuUtilization::for_current_host(),
            filesystems: Filesystems::new(),
        }
    }
}

impl Default for SystemHostFactsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl HostFactsAdapter for SystemHostFactsAdapter {
    fn collect_host_facts(&self) -> Result<HostFacts, UnavailableReason> {
        // Refresh only the CPU list and memory; a full refresh enumerates every process.
        let system = System::new_with_specifics(
            RefreshKind::nothing()
                .with_cpu(CpuRefreshKind::nothing())
                .with_memory(MemoryRefreshKind::everything()),
        )
        .map_err(|_| UnavailableReason::CollectionFailed)?;
        let host_name = System::host_name().map_err(|_| UnavailableReason::Unsupported)?;
        let operating_system_or_kernel = System::long_os_version()
            .or_else(|_| System::kernel_version())
            .map_err(|_| UnavailableReason::Unsupported)?;
        let cpu_count = u32::try_from(system.cpus().len())
            .ok()
            .filter(|count| *count > 0)
            .ok_or(UnavailableReason::CollectionFailed)?;
        let memory = HostMemory::from_system(&system).ok_or(UnavailableReason::CollectionFailed)?;

        Ok(HostFacts {
            host_name,
            operating_system_or_kernel,
            cpu_count,
            total_memory_bytes: memory.total_bytes,
            available_memory_bytes: memory.available_bytes,
            cached_memory_bytes: memory.cached_bytes,
            filesystems: self.filesystems.discover(),
        })
    }

    fn sample(&self) -> HostSample {
        HostSample {
            memory: HostMemory::read().map(|memory| HostMemorySample {
                total_bytes: memory.total_bytes,
                available_bytes: memory.available_bytes,
                cached_bytes: memory.cached_bytes,
            }),
            cpu_utilization: self.cpu.sample(),
            filesystems: self.filesystems.sample(),
        }
    }
}

impl AcceleratorVendorDiscoverer for SystemNvidiaAcceleratorAdapter {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn discover(&self) -> AcceleratorDiscovery {
        match self.discover_nvidia() {
            Ok(Some(poller)) => AcceleratorDiscovery::Detected(Box::new(poller)),
            Ok(None) => AcceleratorDiscovery::Absent,
            Err(reason) => AcceleratorDiscovery::Unavailable(reason),
        }
    }
}

impl AcceleratorVendorAdapter for SystemNvidiaAcceleratorAdapter {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        let inventory = (self.open_inventory)()?;
        let device_count = inventory.device_count()?;
        if device_count == 0 {
            return Err(AcceleratorUnavailable::stable(UnavailableReason::NoDevice));
        }
        let driver_version = inventory.driver_version()?;
        (0..device_count)
            .map(|index| {
                let memory = NvidiaMemoryFacts::from(inventory.device_memory(index)?);
                Ok(AcceleratorFacts {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    model: inventory.device_model(index)?,
                    memory_architecture: memory.architecture as i32,
                    total_memory_bytes: memory.total_bytes,
                    driver_or_runtime_version: driver_version.clone(),
                    device_id: inventory.device_uuid(index)?,
                    vendor_index: Some(index),
                    used_memory_bytes: memory.used_bytes,
                    working_set_limit_bytes: None,
                    // One-shot probes report inventory, not the per-second signals.
                    supported_signals: Vec::new(),
                })
            })
            .collect()
    }
}

struct NvidiaAcceleratorPoller {
    inventory: Arc<dyn NvidiaObservationSource>,
    devices: Vec<NvidiaDevice>,
    processes: Arc<dyn ProcessTable>,
}

/// One NVIDIA device fixed at discovery: its NVML enumeration index, its UUID, and the signals it
/// reported supporting when probed.
struct NvidiaDevice {
    index: u32,
    uuid: String,
    supported_signals: Vec<AcceleratorSignal>,
}

impl NvidiaAcceleratorPoller {
    fn start_token(&self, process_id: u32) -> Option<u64> {
        self.processes
            .entry(process_id)
            .ok()
            .flatten()
            .map(|entry| entry.start_token)
    }
}

impl AcceleratorVendorPoller for NvidiaAcceleratorPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        let driver_version = self.inventory.driver_version()?;
        let mut facts = Vec::with_capacity(self.devices.len());
        let mut processes = Vec::new();
        for device in &self.devices {
            let device_id = &device.uuid;
            let memory = NvidiaMemoryFacts::from(self.inventory.device_memory_by_uuid(device_id)?);
            facts.push(AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: self.inventory.device_model_by_uuid(device_id)?,
                memory_architecture: memory.architecture as i32,
                total_memory_bytes: memory.total_bytes,
                driver_or_runtime_version: driver_version.clone(),
                device_id: device_id.clone(),
                vendor_index: Some(device.index),
                used_memory_bytes: memory.used_bytes,
                working_set_limit_bytes: None,
                supported_signals: device
                    .supported_signals
                    .iter()
                    .map(|signal| *signal as i32)
                    .collect(),
            });
            let candidate_start_tokens = self
                .inventory
                .device_compute_processes_by_uuid(device_id)?
                .into_iter()
                .filter_map(|process| {
                    self.start_token(process.process_id)
                        .map(|start_token| (process.process_id, start_token))
                })
                .collect::<BTreeMap<_, _>>();
            for process in self.inventory.device_compute_processes_by_uuid(device_id)? {
                let Some(process_start_token) = candidate_start_tokens.get(&process.process_id)
                else {
                    continue;
                };
                let Some(validated_start_token) = self.start_token(process.process_id) else {
                    continue;
                };
                if validated_start_token != *process_start_token {
                    continue;
                }
                let leaf = ProcessLifetime {
                    process_id: process.process_id,
                    start_token: validated_start_token,
                };
                // Unreadable ancestry only prevents attribution to a parent workload; the row
                // itself remains valid evidence.
                let ancestors =
                    process_ancestors(self.processes.as_ref(), leaf).unwrap_or_default();
                processes.push(ObservedAcceleratorProcess {
                    device_id: device_id.clone(),
                    process_id: process.process_id,
                    process_start_token: validated_start_token,
                    ancestors,
                    used_memory_bytes: process.used_memory_bytes,
                });
            }
        }
        Ok(AcceleratorVendorObservation { facts, processes })
    }

    fn sampler(&self) -> Option<Arc<dyn AcceleratorSampler>> {
        Some(Arc::new(NvidiaSampler {
            inventory: Arc::clone(&self.inventory),
            device_ids: self
                .devices
                .iter()
                .map(|device| device.uuid.clone())
                .collect(),
        }))
    }
}

/// Reads the signals of fixed NVIDIA devices by UUID, one device lookup each; NVML calls are
/// thread-safe, so it runs alongside a poll.
struct NvidiaSampler {
    inventory: Arc<dyn NvidiaObservationSource>,
    device_ids: Vec<String>,
}

impl AcceleratorSampler for NvidiaSampler {
    fn sample(&self) -> Vec<AcceleratorSample> {
        self.device_ids
            .iter()
            .filter_map(|device_id| {
                let sample = self
                    .inventory
                    .device_sample_by_uuid(device_id)
                    .inspect_err(|error| {
                        debug!(device_id, error = %error, "accelerator sample unavailable");
                    })
                    .ok()?;
                let memory =
                    signal_reading(device_id, "memory", sample.memory).map(NvidiaMemoryFacts::from);
                Some(AcceleratorSample {
                    memory_total_bytes: memory.as_ref().and_then(|memory| memory.total_bytes),
                    memory_used_bytes: memory.as_ref().and_then(|memory| memory.used_bytes),
                    utilization: signal_reading(
                        device_id,
                        "utilization",
                        sample.utilization_percent,
                    )
                    .map(|percent| f64::from(percent) / 100.0),
                    temperature_celsius: signal_reading(
                        device_id,
                        "temperature",
                        sample.temperature_celsius,
                    )
                    .map(f64::from),
                    power_watts: signal_reading(device_id, "power", sample.power_milliwatts)
                        .map(|milliwatts| f64::from(milliwatts) / 1_000.0),
                    ..AcceleratorSample::new(device_id.clone())
                })
            })
            .collect()
    }
}

/// The signals a device supports, from one probe at discovery: only a signal the device reports
/// as not supported is left out, so a transient failure keeps the signal, shown as a gap while it
/// fails.
fn probe_supported_signals(
    inventory: &dyn NvidiaObservationSource,
    device_uuid: &str,
) -> Vec<AcceleratorSignal> {
    let unsupported = |result: Result<(), &dotcore_nvml::Unavailable>| {
        result.is_err_and(|error| error.kind() == dotcore_nvml::UnavailableKind::Unsupported)
    };
    let probe = inventory.device_sample_by_uuid(device_uuid);
    let readings = match &probe {
        Ok(sample) => [
            (
                AcceleratorSignal::Utilization,
                sample.utilization_percent.as_ref().map(|_| ()),
            ),
            (
                AcceleratorSignal::Temperature,
                sample.temperature_celsius.as_ref().map(|_| ()),
            ),
            (
                AcceleratorSignal::Power,
                sample.power_milliwatts.as_ref().map(|_| ()),
            ),
        ],
        Err(error) => [
            (AcceleratorSignal::Utilization, Err(error)),
            (AcceleratorSignal::Temperature, Err(error)),
            (AcceleratorSignal::Power, Err(error)),
        ],
    };
    readings
        .into_iter()
        .filter(|(_, result)| !unsupported(*result))
        .map(|(signal, _)| signal)
        .collect()
}

/// One signal reading, absent when it failed; a failure other than not supported is logged, since
/// a supported signal should read.
fn signal_reading<T>(
    device_id: &str,
    signal: &'static str,
    reading: Result<T, dotcore_nvml::Unavailable>,
) -> Option<T> {
    reading
        .inspect_err(|error| {
            if error.kind() != dotcore_nvml::UnavailableKind::Unsupported {
                debug!(device_id, signal, error = %error, "accelerator signal unavailable");
            }
        })
        .ok()
}

impl AcceleratorVendorAdapter for SystemAmdAcceleratorAdapter {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Amd
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        let driver_version = self.driver_version()?;
        let mut facts: Vec<_> = self
            .cards()?
            .into_iter()
            .map(|card| {
                let total_memory_bytes = card.vram_total();
                AcceleratorFacts {
                    vendor: AcceleratorVendor::Amd as i32,
                    model: card.model,
                    memory_architecture: if total_memory_bytes.is_some() {
                        AcceleratorMemoryArchitecture::Dedicated
                    } else {
                        AcceleratorMemoryArchitecture::Unspecified
                    } as i32,
                    total_memory_bytes,
                    driver_or_runtime_version: driver_version.clone(),
                    device_id: card.unique_id,
                    // DRM card numbers are kernel ordering, not a vendor-native enumeration index.
                    vendor_index: None,
                    used_memory_bytes: None,
                    working_set_limit_bytes: None,
                    supported_signals: Vec::new(),
                }
            })
            .collect();
        facts.sort_by(|a, b| a.model.cmp(&b.model));
        if facts.is_empty() {
            return Err(AcceleratorUnavailable::stable(UnavailableReason::NoDevice));
        }
        Ok(facts)
    }
}

/// One AMD DRM card: its sysfs device directory, PCI model, and `unique_id`, empty when the card
/// exposes none.
struct AmdCard {
    device_root: PathBuf,
    model: String,
    unique_id: String,
}

impl AmdCard {
    fn vram_total(&self) -> Option<u64> {
        read_u64(&self.device_root.join("mem_info_vram_total"))
            .ok()
            .flatten()
    }
}

const AMD_PCI_VENDOR_ID: &str = "1002";

fn accelerator_observation(
    vendor: AcceleratorVendor,
    result: Result<Vec<AcceleratorFacts>, AcceleratorUnavailable>,
) -> AcceleratorObservation {
    let observation = match result {
        Ok(facts) => accelerator_observation::Result::Facts(AcceleratorFactsSet { facts }),
        Err(error) => accelerator_observation::Result::Unavailable(unavailable(
            error.reason(),
            vendor_source(vendor),
        )),
    };
    AcceleratorObservation {
        vendor: vendor as i32,
        result: Some(observation),
    }
}

fn record_observation<T>(
    span: &Span,
    component: &'static str,
    result: &Result<T, UnavailableReason>,
) {
    if let Err(reason) = result {
        span.record("component", component);
        span.record("error_kind", format!("{reason:?}"));
        warn!(parent: span, component, error_kind = ?reason, "hardware observation unavailable");
    }
}

fn host_observation(result: Result<HostFacts, UnavailableReason>) -> HostObservation {
    match result {
        Ok(facts) => HostObservation {
            result: Some(host_observation::Result::Facts(facts)),
        },
        Err(reason) => HostObservation {
            result: Some(host_observation::Result::Unavailable(unavailable(
                reason, "Host",
            ))),
        },
    }
}

fn unavailable(reason: UnavailableReason, source: &str) -> ObservationUnavailable {
    let (code, message) = match reason {
        UnavailableReason::Unsupported => (
            "UNSUPPORTED",
            format!("{source} observation is unavailable because this host does not support it."),
        ),
        UnavailableReason::PermissionDenied => (
            "PERMISSION_DENIED",
            format!("{source} observation is unavailable because access was denied."),
        ),
        UnavailableReason::MissingLibrary => (
            "MISSING_LIBRARY",
            format!("{source} observation is unavailable because its library is not available."),
        ),
        UnavailableReason::NoDevice => (
            "NO_DEVICE",
            format!("{source} observation is unavailable because no device is present."),
        ),
        UnavailableReason::CollectionFailed => (
            "COLLECTION_FAILED",
            format!("{source} observation is unavailable because collection failed."),
        ),
    };
    ObservationUnavailable {
        code: code.to_owned(),
        message,
    }
}

fn vendor_source(vendor: AcceleratorVendor) -> &'static str {
    match vendor {
        AcceleratorVendor::Unspecified => "Accelerator",
        AcceleratorVendor::Nvidia => "NVIDIA",
        AcceleratorVendor::Amd => "AMD",
        AcceleratorVendor::Apple => "Apple",
    }
}

fn vendor_component(vendor: AcceleratorVendor) -> &'static str {
    match vendor {
        AcceleratorVendor::Unspecified => "accelerator",
        AcceleratorVendor::Nvidia => "nvidia",
        AcceleratorVendor::Amd => "amd",
        AcceleratorVendor::Apple => "apple",
    }
}

impl From<UnavailableReason> for AcceleratorUnavailable {
    fn from(reason: UnavailableReason) -> Self {
        Self::stable(reason)
    }
}

impl From<dotcore_nvml::Unavailable> for AcceleratorUnavailable {
    fn from(error: dotcore_nvml::Unavailable) -> Self {
        let reason = unavailable_reason_for_nvml_kind(error.kind());
        Self::with_source(reason, error)
    }
}

impl From<dotcore_nvml::Unavailable> for UnavailableReason {
    fn from(error: dotcore_nvml::Unavailable) -> Self {
        unavailable_reason_for_nvml_kind(error.kind())
    }
}

fn unavailable_reason_for_nvml_kind(kind: dotcore_nvml::UnavailableKind) -> UnavailableReason {
    match kind {
        dotcore_nvml::UnavailableKind::MissingLibrary => UnavailableReason::MissingLibrary,
        dotcore_nvml::UnavailableKind::PermissionDenied => UnavailableReason::PermissionDenied,
        dotcore_nvml::UnavailableKind::Unsupported => UnavailableReason::Unsupported,
        dotcore_nvml::UnavailableKind::Initialization => UnavailableReason::CollectionFailed,
    }
}

fn unavailable_reason_for_io_error(kind: std::io::ErrorKind) -> UnavailableReason {
    match kind {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            UnavailableReason::NoDevice
        }
        std::io::ErrorKind::PermissionDenied => UnavailableReason::PermissionDenied,
        _ => UnavailableReason::CollectionFailed,
    }
}

/// Protocol memory fields for one NVIDIA device.
struct NvidiaMemoryFacts {
    architecture: AcceleratorMemoryArchitecture,
    total_bytes: Option<u64>,
    used_bytes: Option<u64>,
}

impl From<dotcore_nvml::DeviceMemory> for NvidiaMemoryFacts {
    fn from(memory: dotcore_nvml::DeviceMemory) -> Self {
        match memory {
            dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes,
                used_bytes,
            } => Self {
                architecture: AcceleratorMemoryArchitecture::Dedicated,
                total_bytes: Some(total_bytes),
                used_bytes: Some(used_bytes),
            },
            // Host memory is never reported as accelerator memory; host facts describe the pool.
            dotcore_nvml::DeviceMemory::Unified => Self {
                architecture: AcceleratorMemoryArchitecture::Unified,
                total_bytes: None,
                used_bytes: None,
            },
        }
    }
}

fn read_trimmed(path: &Path) -> Result<String, std::io::Error> {
    Ok(fs::read_to_string(path)?.trim().to_owned())
}

fn read_u64(path: &Path) -> Result<Option<u64>, std::io::Error> {
    Ok(read_trimmed(path)?.parse::<u64>().ok())
}

/// Parses a sysfs PCI identifier (`0x1002`) into its canonical lowercase form.
fn pci_id(path: &Path) -> Result<Option<String>, UnavailableReason> {
    let raw = read_trimmed(path).map_err(|error| unavailable_reason_for_io_error(error.kind()))?;
    let hex = raw.strip_prefix("0x").unwrap_or(&raw);
    let Ok(value) = u64::from_str_radix(hex, 16) else {
        return Ok(None);
    };
    Ok(Some(format!("{value:04x}")))
}

#[cfg(test)]
mod tests;
