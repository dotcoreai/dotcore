//! Single-owner boundary over the NVIDIA management library (NVML).
//!
//! This crate is the only backend crate that links the NVML wrapper, exposing a
//! process-wide handle that is loaded at most once along with a small set of
//! read-only device facts and per-second device readings. Vendor-library
//! discovery happens in code against a fixed list of known locations, so
//! consumers never rely on process environment or OS packaging to find the
//! library.

use std::sync::{Arc, OnceLock};

pub use nvml_wrapper::{Nvml, enums::device::UsedGpuMemory};
use nvml_wrapper::{
    enum_wrappers::device::TemperatureSensor,
    error::NvmlError,
    struct_wrappers::device::{AccountingStats, MemoryInfo, ProcessInfo, Utilization},
};
use thiserror::Error;

const STANDARD_NVML_LIBRARY: &str = "libnvidia-ml.so.1";

/// NVML compatibility locations for x86_64 Linux hosts.
pub const X86_64_NVML_LIBRARY_SEARCH_PATHS: &[&str] = &[
    "/run/opengl-driver/lib/libnvidia-ml.so.1",
    "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
    "/usr/lib/x86_64-linux-gnu/nvidia/current/libnvidia-ml.so.1",
    "/usr/lib64/libnvidia-ml.so.1",
    "/usr/lib/libnvidia-ml.so.1",
];

/// NVML compatibility locations for aarch64 Linux hosts.
pub const AARCH64_NVML_LIBRARY_SEARCH_PATHS: &[&str] = &[
    "/run/opengl-driver/lib/libnvidia-ml.so.1",
    "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
    "/usr/lib/aarch64-linux-gnu/nvidia/current/libnvidia-ml.so.1",
    "/usr/lib64/libnvidia-ml.so.1",
    "/usr/lib/libnvidia-ml.so.1",
];

/// Vendor-managed NVML library locations that the standard dynamic loader misses.
#[cfg(target_arch = "x86_64")]
pub const NVML_LIBRARY_SEARCH_PATHS: &[&str] = X86_64_NVML_LIBRARY_SEARCH_PATHS;

/// Vendor-managed NVML library locations that the standard dynamic loader misses.
#[cfg(target_arch = "aarch64")]
pub const NVML_LIBRARY_SEARCH_PATHS: &[&str] = AARCH64_NVML_LIBRARY_SEARCH_PATHS;

/// Vendor-managed NVML library locations for unsupported build architectures.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub const NVML_LIBRARY_SEARCH_PATHS: &[&str] = &[];

/// Stable causes for NVML being unavailable.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum UnavailableKind {
    /// The NVML library could not be loaded from any known location.
    #[error("the NVML library is not available")]
    MissingLibrary,

    /// NVML access was denied by the host.
    #[error("NVML access was denied")]
    PermissionDenied,

    /// The loaded NVML interface is not supported by this host.
    #[error("NVML is not supported by this host")]
    Unsupported,

    /// The NVML library loaded but the driver could not be initialized.
    #[error("NVML driver initialization failed")]
    Initialization,
}

/// Typed NVML unavailability with an optional discovery diagnostic.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{kind}")]
pub struct Unavailable {
    kind: UnavailableKind,
    #[source]
    diagnostic: Option<Arc<LibraryDiscoveryDiagnostic>>,
}

impl Unavailable {
    /// Returns the stable category used by callers and public observations.
    pub fn kind(&self) -> UnavailableKind {
        self.kind
    }

    /// Creates an unavailable result with no discovery diagnostic.
    pub fn from_kind(kind: UnavailableKind) -> Self {
        Self {
            kind,
            diagnostic: None,
        }
    }

    fn missing_library(attempts: Vec<LibraryLoadAttempt>) -> Self {
        Self {
            kind: UnavailableKind::MissingLibrary,
            diagnostic: Some(Arc::new(LibraryDiscoveryDiagnostic::new(attempts))),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("NVML library discovery failed: {summary}")]
struct LibraryDiscoveryDiagnostic {
    attempts: Box<[LibraryLoadAttempt]>,
    summary: String,
}

impl LibraryDiscoveryDiagnostic {
    fn new(attempts: Vec<LibraryLoadAttempt>) -> Self {
        let summary = attempts
            .iter()
            .map(|attempt| format!("{}: {}", attempt.location, attempt.detail))
            .collect::<Vec<_>>()
            .join("; ");
        Self {
            attempts: attempts.into_boxed_slice(),
            summary,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LibraryLoadAttempt {
    location: &'static str,
    detail: String,
}

impl LibraryLoadAttempt {
    fn new(location: &'static str, detail: String) -> Self {
        Self { location, detail }
    }
}

/// Device memory observed through one NVML query, classified by memory architecture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceMemory {
    /// The device owns a dedicated memory pool with these totals.
    Dedicated { total_bytes: u64, used_bytes: u64 },

    /// The device allocates from host memory, so NVML reports no dedicated totals.
    Unified,
}

/// Readings of one NVIDIA device from one device lookup. Each reading fails on its own, with
/// [`UnavailableKind::Unsupported`] when the device does not support it, so one failing query
/// never hides the others.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceSample {
    pub memory: Result<DeviceMemory, Unavailable>,

    /// Percent of time over the driver's last sample period during which one or more kernels ran.
    pub utilization_percent: Result<u32, Unavailable>,

    /// GPU die temperature in degrees Celsius.
    pub temperature_celsius: Result<u32, Unavailable>,

    /// Power draw in milliwatts as the driver reports it: averaged over one second on Ampere and
    /// newer, instantaneous on older devices, and the GPU alone on parts sharing a package with
    /// the CPU, such as GB10.
    pub power_milliwatts: Result<u32, Unavailable>,
}

/// Memory attributed by NVML to one compute process on one device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComputeProcessMemory {
    pub process_id: u32,
    pub used_memory_bytes: Option<u64>,
}

/// Optional process-lifetime accounting evidence reported by NVML.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessAccounting {
    pub max_memory_usage_bytes: Option<u64>,
    pub start_unix_micros: u64,
    pub is_running: bool,
}
static NVML: OnceLock<Result<Nvml, Unavailable>> = OnceLock::new();

/// Opens the process-wide NVML handle, loading the library at most once.
pub fn open() -> Result<&'static Nvml, Unavailable> {
    open_cached_with(&NVML, || open_with_loader(&SystemLibraryLoader))
}

/// Reads the number of NVIDIA devices through an open handle.
pub fn device_count(nvml: &Nvml) -> Result<u32, Unavailable> {
    nvml.device_count().map_err(unavailable_for_nvml_error)
}

/// Reads the NVIDIA driver version through an open handle.
pub fn driver_version(nvml: &Nvml) -> Result<String, Unavailable> {
    nvml.sys_driver_version()
        .map_err(unavailable_for_nvml_error)
}

/// Reads the CUDA driver version through an open handle.
///
/// The current NVML entry point is preferred, and the legacy entry point of the same library
/// answers when a host driver does not support it. A host whose driver reports neither version is
/// reported as the preferred call's failure, which keeps an unsupported query from failing an
/// observation that needs no version at all.
pub fn cuda_driver_version(nvml: &Nvml) -> Result<i32, Unavailable> {
    cuda_driver_version_with(nvml.sys_cuda_driver_version(), || {
        legacy_cuda_driver_version(nvml)
    })
}

/// Reads the model name of one NVIDIA device through an open handle.
pub fn device_model(nvml: &Nvml, index: u32) -> Result<String, Unavailable> {
    nvml.device_by_index(index)
        .map_err(unavailable_for_nvml_error)?
        .name()
        .map_err(unavailable_for_nvml_error)
}

/// Reads the immutable UUID of one physical NVIDIA device through an open handle.
pub fn device_uuid(nvml: &Nvml, index: u32) -> Result<String, Unavailable> {
    nvml.device_by_index(index)
        .map_err(unavailable_for_nvml_error)?
        .uuid()
        .map_err(unavailable_for_nvml_error)
}

/// Reads the model name of one NVIDIA device by immutable UUID.
pub fn device_model_by_uuid(nvml: &Nvml, device_uuid: &str) -> Result<String, Unavailable> {
    nvml.device_by_uuid(device_uuid)
        .map_err(unavailable_for_nvml_error)?
        .name()
        .map_err(unavailable_for_nvml_error)
}

/// Reads dedicated total and raw used memory for one NVIDIA device through one NVML query.
/// Raw used memory includes memory reserved by the driver and is therefore derived as total minus free.
/// A device whose memory query is unsupported after a successful lookup, such as GB10, is unified.
pub fn device_memory(nvml: &Nvml, index: u32) -> Result<DeviceMemory, Unavailable> {
    let device = nvml
        .device_by_index(index)
        .map_err(unavailable_for_nvml_error)?;
    classify_device_memory(device.memory_info())
}

/// Reads dedicated total and raw used memory for one NVIDIA device by immutable UUID.
pub fn device_memory_by_uuid(nvml: &Nvml, device_uuid: &str) -> Result<DeviceMemory, Unavailable> {
    let device = nvml
        .device_by_uuid(device_uuid)
        .map_err(unavailable_for_nvml_error)?;
    classify_device_memory(device.memory_info())
}

/// Reads memory, utilization, temperature and power of one NVIDIA device by immutable UUID through
/// one device lookup; only a failed lookup fails the whole sample.
pub fn device_sample_by_uuid(nvml: &Nvml, device_uuid: &str) -> Result<DeviceSample, Unavailable> {
    let device = nvml
        .device_by_uuid(device_uuid)
        .map_err(unavailable_for_nvml_error)?;
    Ok(device_sample_from(
        device.memory_info(),
        device.utilization_rates(),
        device.temperature(TemperatureSensor::Gpu),
        device.power_usage(),
    ))
}

/// Reads current compute-process memory for one NVIDIA device.
pub fn device_compute_processes(
    nvml: &Nvml,
    index: u32,
) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
    let processes = nvml
        .device_by_index(index)
        .map_err(unavailable_for_nvml_error)?
        .running_compute_processes()
        .map_err(unavailable_for_nvml_error)?;
    Ok(processes
        .into_iter()
        .map(compute_process_memory_from)
        .collect())
}

/// Reads current compute-process memory for one NVIDIA device by immutable UUID.
pub fn device_compute_processes_by_uuid(
    nvml: &Nvml,
    device_uuid: &str,
) -> Result<Vec<ComputeProcessMemory>, Unavailable> {
    let processes = nvml
        .device_by_uuid(device_uuid)
        .map_err(unavailable_for_nvml_error)?
        .running_compute_processes()
        .map_err(unavailable_for_nvml_error)?;
    Ok(processes
        .into_iter()
        .map(compute_process_memory_from)
        .collect())
}

/// Reports whether process accounting is enabled for one NVIDIA device.
pub fn device_accounting_enabled(nvml: &Nvml, index: u32) -> Result<bool, Unavailable> {
    nvml.device_by_index(index)
        .map_err(unavailable_for_nvml_error)?
        .is_accounting_enabled()
        .map_err(unavailable_for_nvml_error)
}

/// Reads process-lifetime accounting for one process on one NVIDIA device.
pub fn device_process_accounting(
    nvml: &Nvml,
    index: u32,
    process_id: u32,
) -> Result<ProcessAccounting, Unavailable> {
    let accounting = nvml
        .device_by_index(index)
        .map_err(unavailable_for_nvml_error)?
        .accounting_stats_for(process_id)
        .map_err(unavailable_for_nvml_error)?;
    Ok(process_accounting_from(accounting))
}

fn classify_device_memory(
    result: Result<MemoryInfo, NvmlError>,
) -> Result<DeviceMemory, Unavailable> {
    match result {
        Ok(memory) => Ok(device_memory_from(memory)),
        // NVIDIA documents this result as expected for devices without a dedicated framebuffer.
        Err(NvmlError::NotSupported) => Ok(DeviceMemory::Unified),
        Err(error) => Err(unavailable_for_nvml_error(error)),
    }
}

fn device_sample_from(
    memory: Result<MemoryInfo, NvmlError>,
    utilization: Result<Utilization, NvmlError>,
    temperature_celsius: Result<u32, NvmlError>,
    power_milliwatts: Result<u32, NvmlError>,
) -> DeviceSample {
    DeviceSample {
        memory: classify_device_memory(memory),
        utilization_percent: utilization
            .map(|utilization| utilization.gpu)
            .map_err(unavailable_for_nvml_error),
        temperature_celsius: temperature_celsius.map_err(unavailable_for_nvml_error),
        power_milliwatts: power_milliwatts.map_err(unavailable_for_nvml_error),
    }
}

fn device_memory_from(memory: MemoryInfo) -> DeviceMemory {
    DeviceMemory::Dedicated {
        total_bytes: memory.total,
        used_bytes: memory.total.saturating_sub(memory.free),
    }
}

fn compute_process_memory_from(process: ProcessInfo) -> ComputeProcessMemory {
    ComputeProcessMemory {
        process_id: process.pid,
        used_memory_bytes: match process.used_gpu_memory {
            UsedGpuMemory::Used(bytes) => Some(bytes),
            UsedGpuMemory::Unavailable => None,
        },
    }
}

fn process_accounting_from(accounting: AccountingStats) -> ProcessAccounting {
    ProcessAccounting {
        max_memory_usage_bytes: accounting.max_memory_usage,
        start_unix_micros: accounting.start_time,
        is_running: accounting.is_running,
    }
}

fn open_with_loader<L>(loader: &L) -> Result<L::Handle, Unavailable>
where
    L: NvmlLibraryLoader,
{
    open_with_initializers(
        || {
            loader
                .initialize_standard()
                .map_err(classify_initialization_error)
        },
        |library_path| {
            loader
                .initialize_with_library(library_path)
                .map_err(classify_initialization_error)
        },
    )
}

fn open_with_initializers<H, S, E>(
    mut initialize_standard: S,
    mut initialize_explicit: E,
) -> Result<H, Unavailable>
where
    S: FnMut() -> Result<H, LibraryInitializationFailure>,
    E: FnMut(&'static str) -> Result<H, LibraryInitializationFailure>,
{
    let mut attempts = Vec::with_capacity(NVML_LIBRARY_SEARCH_PATHS.len() + 1);
    match initialize_standard() {
        Ok(handle) => return Ok(handle),
        Err(LibraryInitializationFailure::Load(detail)) => {
            attempts.push(LibraryLoadAttempt::new(STANDARD_NVML_LIBRARY, detail));
        }
        Err(LibraryInitializationFailure::Terminal(error)) => return Err(error),
    }

    for &library_path in NVML_LIBRARY_SEARCH_PATHS {
        match initialize_explicit(library_path) {
            Ok(handle) => return Ok(handle),
            Err(LibraryInitializationFailure::Load(detail)) => {
                attempts.push(LibraryLoadAttempt::new(library_path, detail));
            }
            Err(LibraryInitializationFailure::Terminal(error)) => return Err(error),
        }
    }

    Err(Unavailable::missing_library(attempts))
}

enum LibraryInitializationFailure {
    Load(String),
    Terminal(Unavailable),
}

fn classify_initialization_error(error: NvmlError) -> LibraryInitializationFailure {
    match error {
        NvmlError::LibloadingError(error) => {
            // libloading's error is not cloneable, while the process-wide cached
            // failure must be reusable by every caller.
            LibraryInitializationFailure::Load(error.to_string())
        }
        error => LibraryInitializationFailure::Terminal(unavailable_for_nvml_error(error)),
    }
}

fn open_cached_with<H, F>(
    cache: &OnceLock<Result<H, Unavailable>>,
    initialize: F,
) -> Result<&H, Unavailable>
where
    F: FnOnce() -> Result<H, Unavailable>,
{
    match cache.get_or_init(initialize) {
        Ok(handle) => Ok(handle),
        Err(error) => Err(error.clone()),
    }
}

/// Loads the NVML library with a controllable initialization sequence.
pub trait NvmlLibraryLoader {
    /// The initialized handle produced by this loader.
    type Handle;

    /// Initializes NVML through the standard dynamic loader.
    fn initialize_standard(&self) -> Result<Self::Handle, NvmlError>;

    /// Initializes NVML from an explicit vendor-managed library path.
    fn initialize_with_library(&self, library_path: &str) -> Result<Self::Handle, NvmlError>;
}

/// Loads NVML through `nvml-wrapper` on the current host.
pub struct SystemLibraryLoader;

impl NvmlLibraryLoader for SystemLibraryLoader {
    type Handle = Nvml;

    fn initialize_standard(&self) -> Result<Self::Handle, NvmlError> {
        Nvml::init()
    }

    fn initialize_with_library(&self, library_path: &str) -> Result<Self::Handle, NvmlError> {
        Nvml::builder().lib_path(library_path.as_ref()).init()
    }
}

/// NVML reports a successful call through a zero return status.
const NVML_SUCCESS: u32 = 0;

/// Applies the legacy CUDA driver query only when the preferred query fails.
fn cuda_driver_version_with(
    preferred: Result<i32, NvmlError>,
    legacy: impl FnOnce() -> Result<i32, Unavailable>,
) -> Result<i32, Unavailable> {
    preferred
        .map_err(unavailable_for_nvml_error)
        .or_else(|preferred| legacy().map_err(|_| preferred))
}

/// Reads the CUDA driver version from the legacy entry point the same NVML library exposes.
fn legacy_cuda_driver_version(nvml: &Nvml) -> Result<i32, Unavailable> {
    let entry_point = nvml
        .lib()
        .nvmlSystemGetCudaDriverVersion
        .as_ref()
        .map_err(|_| Unavailable::from_kind(UnavailableKind::MissingLibrary))?;
    let mut version: i32 = 0;
    // SAFETY: the entry point writes one version value through the pointer it receives and reports
    // its status in its return value, and the pointer outlives the call.
    let status = unsafe { (*entry_point)(&mut version) };
    if status == NVML_SUCCESS {
        Ok(version)
    } else {
        Err(Unavailable::from_kind(UnavailableKind::Initialization))
    }
}

fn unavailable_for_nvml_error(error: NvmlError) -> Unavailable {
    let kind = match error {
        NvmlError::NotSupported => UnavailableKind::Unsupported,
        NvmlError::NoPermission => UnavailableKind::PermissionDenied,
        NvmlError::LibloadingError(_) | NvmlError::LibraryNotFound => {
            UnavailableKind::MissingLibrary
        }
        _ => UnavailableKind::Initialization,
    };
    Unavailable::from_kind(kind)
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::VecDeque, ffi::OsStr};

    use super::*;

    #[test]
    fn cuda_driver_version_prefers_the_current_entry_point_and_falls_back_to_the_legacy_one() {
        // A failing legacy result proves the preferred success is returned untouched.
        assert_eq!(
            cuda_driver_version_with(Ok(13_000), || Err(Unavailable::from_kind(
                UnavailableKind::MissingLibrary
            ))),
            Ok(13_000)
        );
        assert_eq!(
            cuda_driver_version_with(Err(NvmlError::NotSupported), || Ok(12_000)),
            Ok(12_000)
        );
        assert_eq!(
            cuda_driver_version_with(Err(NvmlError::NotSupported), || Err(
                Unavailable::from_kind(UnavailableKind::Initialization)
            ))
            .err()
            .map(|error| error.kind()),
            Some(UnavailableKind::Unsupported)
        );
    }

    struct FakeLibraryLoader {
        standard_result: RefCell<Option<Result<(), NvmlError>>>,
        library_results: RefCell<VecDeque<Result<(), NvmlError>>>,
        attempts: RefCell<Vec<String>>,
    }

    impl FakeLibraryLoader {
        fn new(
            standard_result: Result<(), NvmlError>,
            library_results: impl IntoIterator<Item = Result<(), NvmlError>>,
        ) -> Self {
            Self {
                standard_result: RefCell::new(Some(standard_result)),
                library_results: RefCell::new(library_results.into_iter().collect()),
                attempts: RefCell::new(Vec::new()),
            }
        }

        fn attempts(&self) -> Vec<String> {
            self.attempts.borrow().clone()
        }
    }

    impl NvmlLibraryLoader for FakeLibraryLoader {
        type Handle = ();

        fn initialize_standard(&self) -> Result<Self::Handle, NvmlError> {
            self.attempts.borrow_mut().push("standard".to_owned());
            self.standard_result
                .borrow_mut()
                .take()
                .expect("standard initialization is attempted once")
        }

        fn initialize_with_library(&self, library_path: &str) -> Result<Self::Handle, NvmlError> {
            self.attempts.borrow_mut().push(library_path.to_owned());
            self.library_results
                .borrow_mut()
                .pop_front()
                .expect("compatibility library was configured")
        }
    }

    const MISSING_TEST_LIBRARY: &str = "/dotcore-test-missing/libnvidia-ml.so.1";

    fn missing_library_error() -> NvmlError {
        Nvml::builder()
            .lib_path(OsStr::new(MISSING_TEST_LIBRARY))
            .init()
            .expect_err("the test-only NVML library path does not exist")
    }

    #[test]
    fn device_memory_includes_driver_reserved_bytes_in_raw_used_memory() {
        let memory = device_memory_from(MemoryInfo {
            free: 600,
            reserved: 100,
            total: 1_000,
            used: 300,
            version: 2,
        });

        assert_eq!(
            memory,
            DeviceMemory::Dedicated {
                total_bytes: 1_000,
                used_bytes: 400,
            }
        );
    }

    #[test]
    fn compute_process_memory_preserves_unknown_values_without_false_zero() {
        assert_eq!(
            compute_process_memory_from(ProcessInfo {
                pid: 41,
                used_gpu_memory: UsedGpuMemory::Used(123_456),
                gpu_instance_id: None,
                compute_instance_id: None,
            }),
            ComputeProcessMemory {
                process_id: 41,
                used_memory_bytes: Some(123_456),
            }
        );
        assert_eq!(
            compute_process_memory_from(ProcessInfo {
                pid: 42,
                used_gpu_memory: UsedGpuMemory::Unavailable,
                gpu_instance_id: None,
                compute_instance_id: None,
            }),
            ComputeProcessMemory {
                process_id: 42,
                used_memory_bytes: None,
            }
        );
    }

    #[test]
    fn standard_discovery_succeeds_before_search_paths_are_tried() {
        let loader = FakeLibraryLoader::new(Ok(()), []);

        assert!(open_with_loader(&loader).is_ok());
        assert_eq!(loader.attempts(), ["standard"]);
    }

    #[test]
    fn each_search_path_can_win_after_prior_load_failures() {
        for success_index in 0..NVML_LIBRARY_SEARCH_PATHS.len() {
            let library_results = (0..success_index)
                .map(|_| Err(missing_library_error()))
                .chain(std::iter::once(Ok(())))
                .collect::<Vec<_>>();
            let loader = FakeLibraryLoader::new(Err(missing_library_error()), library_results);

            assert!(open_with_loader(&loader).is_ok());
            assert_eq!(
                loader.attempts(),
                std::iter::once("standard".to_owned())
                    .chain(
                        NVML_LIBRARY_SEARCH_PATHS
                            .iter()
                            .take(success_index + 1)
                            .map(|path| (*path).to_owned())
                    )
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn initialization_errors_do_not_try_search_paths() {
        for error in [
            NvmlError::DriverNotLoaded,
            NvmlError::FailedToLoadSymbol("nvmlInit_v2".to_owned()),
            NvmlError::LibRmVersionMismatch,
        ] {
            let loader = FakeLibraryLoader::new(Err(error), [Ok(())]);

            assert_eq!(
                open_with_loader(&loader)
                    .expect_err("initialization should fail")
                    .kind(),
                UnavailableKind::Initialization
            );
            assert_eq!(loader.attempts(), ["standard"]);
        }
    }

    #[test]
    fn device_memory_classifies_only_unsupported_memory_info_as_unified() {
        assert_eq!(
            classify_device_memory(Ok(MemoryInfo {
                free: 28,
                reserved: 0,
                total: 128,
                used: 100,
                version: 2,
            })),
            Ok(DeviceMemory::Dedicated {
                total_bytes: 128,
                used_bytes: 100,
            })
        );
        assert_eq!(
            classify_device_memory(Err(NvmlError::NotSupported)),
            Ok(DeviceMemory::Unified)
        );
        assert_eq!(
            classify_device_memory(Err(NvmlError::NoPermission))
                .expect_err("permission failure remains terminal")
                .kind(),
            UnavailableKind::PermissionDenied
        );
    }

    #[test]
    fn device_sample_readings_fail_independently_and_keep_unsupported_distinct() {
        let sample = device_sample_from(
            Err(NvmlError::NotSupported),
            Ok(Utilization {
                gpu: 87,
                memory: 40,
            }),
            Err(NvmlError::GpuLost),
            Err(NvmlError::NotSupported),
        );

        assert_eq!(
            sample,
            DeviceSample {
                memory: Ok(DeviceMemory::Unified),
                utilization_percent: Ok(87),
                temperature_celsius: Err(Unavailable::from_kind(UnavailableKind::Initialization)),
                power_milliwatts: Err(Unavailable::from_kind(UnavailableKind::Unsupported)),
            }
        );
    }

    #[test]
    fn library_not_found_does_not_try_search_paths() {
        let loader = FakeLibraryLoader::new(Err(NvmlError::LibraryNotFound), [Ok(())]);

        assert_eq!(
            open_with_loader(&loader)
                .expect_err("dependent driver library should be terminal")
                .kind(),
            UnavailableKind::MissingLibrary
        );
        assert_eq!(loader.attempts(), ["standard"]);
    }

    #[test]
    fn candidate_non_load_errors_stop_search() {
        let loader = FakeLibraryLoader::new(
            Err(missing_library_error()),
            [Err(NvmlError::NoPermission), Ok(())],
        );

        assert_eq!(
            open_with_loader(&loader)
                .expect_err("permission failure should be terminal")
                .kind(),
            UnavailableKind::PermissionDenied
        );
        assert_eq!(
            loader.attempts(),
            ["standard", NVML_LIBRARY_SEARCH_PATHS[0]]
        );
    }

    #[test]
    fn candidate_library_not_found_is_terminal() {
        let loader = FakeLibraryLoader::new(
            Err(missing_library_error()),
            [Err(NvmlError::LibraryNotFound), Ok(())],
        );

        assert_eq!(
            open_with_loader(&loader)
                .expect_err("dependent driver library should stop compatibility search")
                .kind(),
            UnavailableKind::MissingLibrary
        );
        assert_eq!(
            loader.attempts(),
            ["standard", NVML_LIBRARY_SEARCH_PATHS[0]]
        );
    }

    #[test]
    fn library_load_failures_exhaust_paths_with_detailed_diagnostics() {
        let candidate_index = std::cell::Cell::new(0);
        let error = open_with_initializers::<(), _, _>(
            || {
                Err(LibraryInitializationFailure::Load(
                    "wrong ELF class: ELFCLASS32".to_owned(),
                ))
            },
            |_| {
                let index = candidate_index.get();
                candidate_index.set(index + 1);
                let detail = if index == 0 {
                    "libcuda.so.1: cannot open shared object file: No such file or directory"
                        .to_owned()
                } else {
                    format!("candidate {index} cannot open shared object file")
                };
                Err(LibraryInitializationFailure::Load(detail))
            },
        )
        .expect_err("all library locations should fail");

        assert_eq!(error.kind(), UnavailableKind::MissingLibrary);
        assert_eq!(candidate_index.get(), NVML_LIBRARY_SEARCH_PATHS.len());
        let diagnostic = std::error::Error::source(&error)
            .expect("missing library retains discovery diagnostics")
            .to_string();
        let expected_summary = std::iter::once(format!(
            "{STANDARD_NVML_LIBRARY}: wrong ELF class: ELFCLASS32"
        ))
        .chain(
            NVML_LIBRARY_SEARCH_PATHS
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    if index == 0 {
                        format!(
                            "{path}: libcuda.so.1: cannot open shared object file: No such file or directory"
                        )
                    } else {
                        format!("{path}: candidate {index} cannot open shared object file")
                    }
                }),
        )
        .collect::<Vec<_>>()
        .join("; ");
        assert_eq!(
            diagnostic,
            format!("NVML library discovery failed: {expected_summary}")
        );

        match classify_initialization_error(missing_library_error()) {
            LibraryInitializationFailure::Load(detail) => {
                assert!(
                    detail.contains(MISSING_TEST_LIBRARY),
                    "loader detail should name the attempted library: {detail}"
                );
            }
            LibraryInitializationFailure::Terminal(_) => {
                panic!("libloading errors must remain retryable")
            }
        }
    }

    #[test]
    fn cached_failures_keep_identical_diagnostics_without_reloading() {
        use std::cell::Cell;

        let cache = OnceLock::new();
        let calls = Cell::new(0);
        let expected = Unavailable::missing_library(vec![LibraryLoadAttempt {
            location: STANDARD_NVML_LIBRARY,
            detail: "test loader failure".to_owned(),
        }]);

        let first = open_cached_with(&cache, || {
            calls.set(calls.get() + 1);
            Err::<(), _>(expected.clone())
        })
        .expect_err("the cached initialization should fail");
        let second = open_cached_with(&cache, || {
            calls.set(calls.get() + 1);
            Ok(())
        })
        .expect_err("the cached failure should be reused");

        assert_eq!(calls.get(), 1);
        assert_eq!(first, expected);
        assert_eq!(second, expected);
        assert_eq!(first.to_string(), second.to_string());
        assert_eq!(
            std::error::Error::source(&first).map(ToString::to_string),
            std::error::Error::source(&second).map(ToString::to_string)
        );
    }

    #[test]
    fn standard_permission_failures_map_to_permission_denied() {
        let loader = FakeLibraryLoader::new(Err(NvmlError::NoPermission), [Ok(())]);

        assert_eq!(
            open_with_loader(&loader)
                .expect_err("permission failure should remain distinct")
                .kind(),
            UnavailableKind::PermissionDenied
        );
        assert_eq!(loader.attempts(), ["standard"]);
    }

    #[test]
    fn standard_unsupported_failures_map_to_unsupported() {
        let loader = FakeLibraryLoader::new(Err(NvmlError::NotSupported), [Ok(())]);

        assert_eq!(
            open_with_loader(&loader)
                .expect_err("unsupported host should remain distinct")
                .kind(),
            UnavailableKind::Unsupported
        );
        assert_eq!(loader.attempts(), ["standard"]);
    }

    #[test]
    fn architecture_search_paths_cover_major_linux_host_layouts() {
        assert_eq!(
            X86_64_NVML_LIBRARY_SEARCH_PATHS,
            [
                "/run/opengl-driver/lib/libnvidia-ml.so.1",
                "/usr/lib/x86_64-linux-gnu/libnvidia-ml.so.1",
                "/usr/lib/x86_64-linux-gnu/nvidia/current/libnvidia-ml.so.1",
                "/usr/lib64/libnvidia-ml.so.1",
                "/usr/lib/libnvidia-ml.so.1",
            ]
        );
        assert_eq!(
            AARCH64_NVML_LIBRARY_SEARCH_PATHS,
            [
                "/run/opengl-driver/lib/libnvidia-ml.so.1",
                "/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1",
                "/usr/lib/aarch64-linux-gnu/nvidia/current/libnvidia-ml.so.1",
                "/usr/lib64/libnvidia-ml.so.1",
                "/usr/lib/libnvidia-ml.so.1",
            ]
        );
        for path in X86_64_NVML_LIBRARY_SEARCH_PATHS
            .iter()
            .chain(AARCH64_NVML_LIBRARY_SEARCH_PATHS)
        {
            assert!(path.starts_with('/'));
            assert!(path.ends_with("/libnvidia-ml.so.1"));
            assert!(!path.starts_with("/usr/local"));
            assert_ne!(*path, "/run/opengl-driver-64/lib/libnvidia-ml.so.1");
        }
    }
}
