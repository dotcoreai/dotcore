//! Bounded sampling of the GPU memory used by one workload's process tree, for
//! development workload VRAM measurement.
//!
//! It locates the single NVIDIA device the workload runs on, counting every process descended
//! from the workload's root process, for example a vLLM engine child of the `vllm serve` process.
//! Other processes may share the device: their memory is reported separately and never counted
//! for the workload. Identity changes, ambiguous devices, or missing workload memory are errors
//! rather than guesses.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use dotcore_host_process::{HostProcessTable, ProcessLifetime, ProcessTable, process_ancestors};
use serde::Serialize;

use crate::{
    host_memory::HostMemory,
    nvidia::{NvidiaObservationSource, SystemNvidiaObservationSource},
};

const MAX_DURATION: Duration = Duration::from_secs(60 * 60);
const MIN_INTERVAL: Duration = Duration::from_millis(20);
const MAX_INTERVAL: Duration = Duration::from_millis(50);

/// Cancellation token shared with one bounded VRAM observation.
/// Clones address the same state so another thread can stop active sampling.
#[derive(Clone, Debug, Default)]
pub struct VramCancellation(Arc<AtomicBool>);

impl VramCancellation {
    /// Requests cancellation of the active observation window.
    /// Active sampling returns an error after observing the request.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Workload VRAM evidence collected during one bounded window.
/// The evidence identifies the workload's root process, the sole NVIDIA device used throughout
/// sampling, and every root-descended process seen on that device.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramObservation {
    pub process_id: u32,
    pub process_start_token: u64,
    pub started_unix_millis: u128,
    pub duration_millis: u128,
    pub interval_millis: u128,
    pub gpu: VramGpuIdentity,
    pub isolated: bool,
    pub samples: Vec<VramSample>,
    pub observed_max_bytes: u64,
    pub unattributed_max_bytes: u64,
    pub processes: Vec<VramProcess>,
    pub other_processes: Vec<VramOtherProcess>,
}

/// NVIDIA device identity attached to one observation.
/// Driver and memory facts come from the same NVML connection used for sampling.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramGpuIdentity {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub driver_version: String,
    pub cuda_driver_version: i32,
    pub memory: VramGpuMemory,
}

/// Memory pool backing the observed NVIDIA device, captured when the observation starts.
/// Unified devices such as GB10 allocate from host memory, so samples consume the shared host pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(
    tag = "architecture",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum VramGpuMemory {
    Dedicated {
        total_bytes: u64,
    },

    Unified {
        host_total_bytes: u64,
        host_available_bytes: u64,
    },
}

/// One VRAM sample: the summed memory of every attributed process on the device, and the known
/// memory of other processes sharing it. Elapsed time is relative to the start of the window.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramSample {
    pub elapsed_millis: u128,
    pub used_bytes: u64,
    pub unattributed_bytes: u64,
}

/// One workload process attributed to the observed device at any point of the window.
/// Accounting covers that process's lifetime; summing processes' peaks only bounds their joint peak.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramProcess {
    pub process_id: u32,
    pub start_token: u64,
    pub accounting: VramAvailability<VramAccounting>,
}

/// One process outside the workload seen on the observed device during the window.
/// NVML may not report its memory, so unattributed totals are a lower bound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramOtherProcess {
    pub process_id: u32,
    pub memory_known: bool,
}

/// Optional process-lifetime NVML accounting evidence.
/// This remains separate from bounded samples because accounting covers the process lifetime.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VramAccounting {
    pub max_memory_usage_bytes: Option<u64>,
    pub process_start_unix_micros: u64,
    pub is_running: bool,
}

/// Availability of optional NVIDIA evidence.
/// Unavailability preserves a concrete reason instead of substituting bounded observations.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum VramAvailability<T> {
    Available { value: T },
    Unavailable { reason: String },
}

/// Samples one workload's process tree for at most one hour, automatically locating its sole
/// NVIDIA device. The call rejects root identity changes, ambiguous attribution, and missing
/// workload memory; other processes on the device are reported, not rejected.
pub fn sample_process(
    process_id: u32,
    duration: Duration,
    interval: Duration,
    cancellation: &VramCancellation,
) -> Result<VramObservation> {
    sample_process_observing(
        process_id,
        None,
        duration,
        interval,
        cancellation,
        &mut |_| Ok(()),
    )
}

/// Samples one workload's process tree on a selected device while reporting each sample to a
/// caller, so callers can synchronize their own evidence without duplicating NVML logic.
#[doc(hidden)]
pub fn sample_process_observing(
    process_id: u32,
    selected_device: Option<u32>,
    duration: Duration,
    interval: Duration,
    cancellation: &VramCancellation,
    observer: &mut dyn FnMut(&VramSample) -> Result<()>,
) -> Result<VramObservation> {
    let source = SystemNvidiaObservationSource::open().context("initialize NVML")?;
    sample_process_from(
        &source,
        &HostProcessTable,
        process_id,
        selected_device,
        SamplingWindow { duration, interval },
        cancellation,
        observer,
    )
}

#[derive(Clone, Copy, Debug)]
struct SamplingWindow {
    duration: Duration,
    interval: Duration,
}

#[derive(Clone, Debug)]
struct ProcessUsage {
    process_id: u32,
    used_bytes: Option<u64>,
}

trait NvidiaSource {
    fn device_indices(&self) -> Result<Vec<u32>>;

    fn device_identity(&self, index: u32) -> Result<VramGpuIdentity>;

    fn process_usage(&self, index: u32) -> Result<Vec<ProcessUsage>>;

    fn accounting(
        &self,
        index: u32,
        process_id: u32,
        process_start_token: u64,
    ) -> Result<VramAvailability<VramAccounting>>;
}

/// Workload processes and other processes found on one device.
#[derive(Debug, Default)]
struct DeviceAttribution {
    workload: Vec<(ProcessLifetime, Option<u64>)>,
    other: Vec<ProcessUsage>,
}

/// Whether a running GPU process belongs to the sampled workload.
enum Membership {
    Workload(ProcessLifetime),

    Other,
}

fn sample_process_from(
    source: &impl NvidiaSource,
    processes: &dyn ProcessTable,
    process_id: u32,
    selected_device: Option<u32>,
    window: SamplingWindow,
    cancellation: &VramCancellation,
    observer: &mut dyn FnMut(&VramSample) -> Result<()>,
) -> Result<VramObservation> {
    validate_window(window.duration, window.interval)?;
    if cancellation.is_cancelled() {
        bail!("VRAM observation was cancelled");
    }

    let root = ProcessLifetime {
        process_id,
        start_token: processes
            .entry(process_id)?
            .with_context(|| format!("PID {process_id} is not running"))?
            .start_token,
    };
    let (device_index, initial) = locate_workload(source, processes, root, selected_device)?;
    let mut attributed = BTreeSet::new();
    let mut others = BTreeMap::new();
    workload_used_bytes(&initial, device_index, &mut attributed)?;
    unattributed_used_bytes(&initial, &mut others)?;

    let gpu = source.device_identity(device_index)?;
    let started_unix_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_millis();
    let started = Instant::now();
    let mut samples = Vec::new();
    while started.elapsed() < window.duration {
        if cancellation.is_cancelled() {
            bail!("VRAM observation was cancelled");
        }
        ensure_root_unchanged(processes, root)?;
        let (_, attribution) = locate_workload(source, processes, root, Some(device_index))?;
        let sample = VramSample {
            elapsed_millis: started.elapsed().as_millis(),
            used_bytes: workload_used_bytes(&attribution, device_index, &mut attributed)?,
            unattributed_bytes: unattributed_used_bytes(&attribution, &mut others)?,
        };
        observer(&sample)?;
        samples.push(sample);
        thread::sleep(
            window
                .interval
                .min(window.duration.saturating_sub(started.elapsed())),
        );
    }

    if cancellation.is_cancelled() {
        bail!("VRAM observation was cancelled");
    }
    ensure_root_unchanged(processes, root)?;
    let (_, final_attribution) = locate_workload(source, processes, root, Some(device_index))?;
    workload_used_bytes(&final_attribution, device_index, &mut attributed)?;
    unattributed_used_bytes(&final_attribution, &mut others)?;
    let observed_max_bytes = samples
        .iter()
        .map(|sample| sample.used_bytes)
        .max()
        .context("VRAM observation produced no samples")?;
    let unattributed_max_bytes = samples
        .iter()
        .map(|sample| sample.unattributed_bytes)
        .max()
        .unwrap_or(0);
    let processes = attributed
        .into_iter()
        .map(|process| {
            Ok(VramProcess {
                process_id: process.process_id,
                start_token: process.start_token,
                accounting: process_accounting(source, device_index, root, process)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(VramObservation {
        process_id,
        process_start_token: root.start_token,
        started_unix_millis,
        duration_millis: started.elapsed().as_millis(),
        interval_millis: window.interval.as_millis(),
        gpu,
        isolated: others.is_empty(),
        samples,
        observed_max_bytes,
        unattributed_max_bytes,
        processes,
        other_processes: others
            .into_iter()
            .map(|(process_id, memory_known)| VramOtherProcess {
                process_id,
                memory_known,
            })
            .collect(),
    })
}

fn ensure_root_unchanged(processes: &dyn ProcessTable, root: ProcessLifetime) -> Result<()> {
    let current = processes
        .entry(root.process_id)?
        .map(|entry| entry.start_token);
    if current != Some(root.start_token) {
        bail!(
            "process identity changed while sampling PID {}",
            root.process_id
        );
    }
    Ok(())
}

fn locate_workload(
    source: &impl NvidiaSource,
    processes: &dyn ProcessTable,
    root: ProcessLifetime,
    selected_device: Option<u32>,
) -> Result<(u32, DeviceAttribution)> {
    let process_id = root.process_id;
    let mut matches = Vec::new();
    for index in source.device_indices()? {
        let attribution = attribute_device(processes, root, &source.process_usage(index)?)?;
        if !attribution.workload.is_empty() {
            matches.push((index, attribution));
        }
    }
    let (device_index, attribution) = match matches.len() {
        0 => bail!("PID {process_id} is not running on an NVIDIA device"),
        1 => matches.pop().expect("one matching device"),
        _ => bail!(
            "PID {process_id} is ambiguously attributed to NVIDIA devices {:?}",
            matches.iter().map(|(index, _)| *index).collect::<Vec<_>>()
        ),
    };
    if let Some(selected_device) = selected_device
        && selected_device != device_index
    {
        bail!(
            "PID {process_id} is running on NVIDIA device {device_index}, not selected device {selected_device}"
        );
    }
    Ok((device_index, attribution))
}

fn attribute_device(
    processes: &dyn ProcessTable,
    root: ProcessLifetime,
    usage: &[ProcessUsage],
) -> Result<DeviceAttribution> {
    let mut attribution = DeviceAttribution::default();
    for process in usage {
        match process_membership(processes, root, process.process_id)? {
            Some(Membership::Workload(lifetime)) => {
                attribution.workload.push((lifetime, process.used_bytes));
            }
            Some(Membership::Other) => attribution.other.push(process.clone()),
            None => {}
        }
    }
    Ok(attribution)
}

fn process_membership(
    processes: &dyn ProcessTable,
    root: ProcessLifetime,
    process_id: u32,
) -> Result<Option<Membership>> {
    // An exited process has no entry; it is dropped from this sample, not counted as another process.
    let Some(entry) = processes.entry(process_id)? else {
        return Ok(None);
    };
    let leaf = ProcessLifetime {
        process_id,
        start_token: entry.start_token,
    };
    let belongs = leaf == root || process_ancestors(processes, leaf)?.contains(&root);

    // Re-reading the leaf brackets the walk, so a PID reused meanwhile is dropped, not misattributed.
    let current = processes.entry(process_id)?.map(|entry| entry.start_token);
    if current != Some(leaf.start_token) {
        return Ok(None);
    }
    Ok(Some(if belongs {
        Membership::Workload(leaf)
    } else {
        Membership::Other
    }))
}

fn workload_used_bytes(
    attribution: &DeviceAttribution,
    device_index: u32,
    attributed: &mut BTreeSet<ProcessLifetime>,
) -> Result<u64> {
    let mut used_bytes = 0_u64;
    for (process, process_used_bytes) in &attribution.workload {
        let process_used_bytes = process_used_bytes.with_context(|| {
            format!(
                "NVML memory is unavailable for PID {} on NVIDIA device {device_index}",
                process.process_id
            )
        })?;
        used_bytes = used_bytes
            .checked_add(process_used_bytes)
            .context("workload VRAM total overflows")?;
        attributed.insert(*process);
    }
    Ok(used_bytes)
}

/// Sums the known memory of processes outside the workload and records each one seen; a process
/// whose memory NVML doesn't report stays marked unknown for the whole window.
fn unattributed_used_bytes(
    attribution: &DeviceAttribution,
    others: &mut BTreeMap<u32, bool>,
) -> Result<u64> {
    let mut used_bytes = 0_u64;
    for process in &attribution.other {
        let memory_known = others.entry(process.process_id).or_insert(true);
        match process.used_bytes {
            Some(process_used_bytes) => {
                used_bytes = used_bytes
                    .checked_add(process_used_bytes)
                    .context("unattributed VRAM total overflows")?;
            }
            None => *memory_known = false,
        }
    }
    Ok(used_bytes)
}

fn process_accounting(
    source: &impl NvidiaSource,
    device_index: u32,
    root: ProcessLifetime,
    process: ProcessLifetime,
) -> Result<VramAvailability<VramAccounting>> {
    let accounting = source.accounting(device_index, process.process_id, process.start_token);
    if process == root {
        return accounting;
    }
    // Workers may exit or be replaced during the window, so their accounting is best effort.
    Ok(
        accounting.unwrap_or_else(|error| VramAvailability::Unavailable {
            reason: format!("{error:#}"),
        }),
    )
}

fn validate_window(duration: Duration, interval: Duration) -> Result<()> {
    if duration.is_zero() {
        bail!("duration must be greater than zero");
    }
    if duration > MAX_DURATION {
        bail!(
            "duration must not exceed {} milliseconds",
            MAX_DURATION.as_millis()
        );
    }
    if !(MIN_INTERVAL..=MAX_INTERVAL).contains(&interval) {
        bail!("interval must be between 20 and 50 milliseconds");
    }
    Ok(())
}

impl NvidiaSource for SystemNvidiaObservationSource {
    fn device_indices(&self) -> Result<Vec<u32>> {
        let count = NvidiaObservationSource::device_count(self).context("count NVIDIA devices")?;
        Ok((0..count).collect())
    }

    fn device_identity(&self, index: u32) -> Result<VramGpuIdentity> {
        Ok(VramGpuIdentity {
            index,
            uuid: NvidiaObservationSource::device_uuid(self, index)
                .context("read NVIDIA device UUID")?,
            name: NvidiaObservationSource::device_model(self, index)
                .context("read NVIDIA device name")?,
            driver_version: NvidiaObservationSource::driver_version(self)
                .context("read NVIDIA driver version")?,
            cuda_driver_version: NvidiaObservationSource::cuda_driver_version(self)
                .context("read CUDA driver version")?,
            memory: gpu_memory(
                NvidiaObservationSource::device_memory(self, index)
                    .context("read NVIDIA device memory")?,
                read_host_memory,
            )?,
        })
    }

    fn process_usage(&self, index: u32) -> Result<Vec<ProcessUsage>> {
        Ok(
            NvidiaObservationSource::device_compute_processes(self, index)
                .context("list NVIDIA compute processes")?
                .into_iter()
                .map(|process| ProcessUsage {
                    process_id: process.process_id,
                    used_bytes: process.used_memory_bytes,
                })
                .collect(),
        )
    }

    fn accounting(
        &self,
        index: u32,
        process_id: u32,
        process_start_token: u64,
    ) -> Result<VramAvailability<VramAccounting>> {
        match NvidiaObservationSource::device_accounting_enabled(self, index) {
            Ok(true) => {
                match NvidiaObservationSource::device_process_accounting(self, index, process_id) {
                    Ok(stats)
                        if accounting_matches_process(
                            process_start_token,
                            stats.start_unix_micros,
                        )? =>
                    {
                        Ok(VramAvailability::Available {
                            value: VramAccounting {
                                max_memory_usage_bytes: stats.max_memory_usage_bytes,
                                process_start_unix_micros: stats.start_unix_micros,
                                is_running: stats.is_running,
                            },
                        })
                    }
                    Ok(_) => bail!("NVML accounting identity does not match PID {process_id}"),
                    Err(error) => Ok(VramAvailability::Unavailable {
                        reason: format!("NVML accounting query failed: {error}"),
                    }),
                }
            }
            Ok(false) => Ok(VramAvailability::Unavailable {
                reason: "NVML accounting is disabled".into(),
            }),
            Err(error) => Ok(VramAvailability::Unavailable {
                reason: format!("NVML accounting support query failed: {error}"),
            }),
        }
    }
}

fn gpu_memory(
    memory: dotcore_nvml::DeviceMemory,
    read_host_memory: impl FnOnce() -> Result<HostMemory>,
) -> Result<VramGpuMemory> {
    match memory {
        dotcore_nvml::DeviceMemory::Dedicated { total_bytes, .. } => {
            Ok(VramGpuMemory::Dedicated { total_bytes })
        }
        dotcore_nvml::DeviceMemory::Unified => {
            let host = read_host_memory().context("read shared host memory")?;
            Ok(VramGpuMemory::Unified {
                host_total_bytes: host.total_bytes,
                host_available_bytes: host.available_bytes,
            })
        }
    }
}

fn read_host_memory() -> Result<HostMemory> {
    HostMemory::read().context("host memory total is unavailable")
}

fn accounting_matches_process(start_token: u64, accounting_start_micros: u64) -> Result<bool> {
    // NVIDIA accounting exists only on Linux, whose start token counts clock ticks since boot.
    let boot_seconds = fs::read_to_string("/proc/stat")
        .context("read kernel boot time")?
        .lines()
        .find_map(|line| line.strip_prefix("btime "))
        .context("kernel boot time is unavailable")?
        .parse::<u64>()
        .context("parse kernel boot time")?;
    // SAFETY: sysconf with _SC_CLK_TCK has no pointer arguments or memory effects.
    let ticks_per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if ticks_per_second <= 0 {
        bail!("kernel clock tick rate is unavailable");
    }
    let process_start_micros = boot_seconds
        .checked_mul(1_000_000)
        .and_then(|value| {
            value.checked_add(start_token.saturating_mul(1_000_000) / ticks_per_second as u64)
        })
        .context("process start timestamp overflows")?;
    Ok(process_start_micros.abs_diff(accounting_start_micros) <= 1_000_000)
}

#[cfg(test)]
mod tests {
    use std::{collections::VecDeque, sync::Mutex};

    use dotcore_host_process::ProcessEntry;
    use dotcore_host_process::test_support::FakeProcessTable;

    use super::*;

    #[test]
    fn dedicated_gpu_memory_reports_device_total_without_reading_host_memory() {
        let memory = gpu_memory(
            dotcore_nvml::DeviceMemory::Dedicated {
                total_bytes: 34_179_219_456,
                used_bytes: 1_000,
            },
            || bail!("host memory is not read for dedicated devices"),
        )
        .unwrap();

        assert_eq!(
            memory,
            VramGpuMemory::Dedicated {
                total_bytes: 34_179_219_456
            }
        );
    }

    #[test]
    fn unified_gpu_memory_reports_shared_host_memory() {
        let memory = gpu_memory(dotcore_nvml::DeviceMemory::Unified, || {
            Ok(HostMemory {
                total_bytes: 130_000_000_000,
                available_bytes: 90_000_000_000,
                cached_bytes: None,
            })
        })
        .unwrap();

        assert_eq!(
            memory,
            VramGpuMemory::Unified {
                host_total_bytes: 130_000_000_000,
                host_available_bytes: 90_000_000_000,
            }
        );
        assert_eq!(
            serde_json::to_value(memory).unwrap(),
            serde_json::json!({
                "architecture": "unified",
                "hostTotalBytes": 130_000_000_000_u64,
                "hostAvailableBytes": 90_000_000_000_u64,
            })
        );
    }

    #[test]
    fn samples_exact_process_memory_and_device_identity() {
        let source = FakeNvidia::single(vec![usage(41, Some(123_456))]);

        let observation = observe(&source, &workload_table()).unwrap();

        assert_eq!(observation.process_id, 41);
        assert_eq!(observation.process_start_token, 700);
        assert_eq!(observation.gpu.index, 0);
        assert_eq!(observation.gpu.uuid, "GPU-fixture-0");
        assert_eq!(observation.gpu.name, "Fixture GPU 0");
        assert_eq!(observation.samples.len(), 1);
        assert_eq!(observation.samples[0].used_bytes, 123_456);
        assert_eq!(observation.observed_max_bytes, 123_456);
        assert!(observation.isolated);
        assert_eq!(observation.samples[0].unattributed_bytes, 0);
        assert_eq!(observation.unattributed_max_bytes, 0);
        assert_eq!(observation.other_processes, Vec::new());
        assert_eq!(process_lifetimes(&observation), vec![(41, 700)]);
    }

    #[test]
    fn samples_a_child_process_when_the_root_is_absent_from_nvidia_devices() {
        let source = FakeNvidia::single(vec![usage(1275, Some(24_000_000_000))]);

        let observation = observe(&source, &workload_table()).unwrap();

        assert_eq!(observation.process_id, 41);
        assert_eq!(observation.samples[0].used_bytes, 24_000_000_000);
        assert_eq!(process_lifetimes(&observation), vec![(1275, 800)]);
    }

    #[test]
    fn sums_the_root_and_its_descendants_on_one_device() {
        let source = FakeNvidia::single(vec![
            usage(41, Some(100)),
            usage(1275, Some(200)),
            usage(1276, Some(300)),
        ]);

        let observation = observe(&source, &workload_table()).unwrap();

        assert_eq!(observation.samples[0].used_bytes, 600);
        assert_eq!(observation.observed_max_bytes, 600);
        assert_eq!(
            process_lifetimes(&observation),
            vec![(41, 700), (1275, 800), (1276, 850)]
        );
    }

    #[test]
    fn rejects_process_absent_from_nvidia_devices() {
        let source = FakeNvidia::single(Vec::new());

        let error = observe(&source, &workload_table()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "PID 41 is not running on an NVIDIA device"
        );
    }

    #[test]
    fn rejects_a_root_that_is_not_running() {
        let source = FakeNvidia::single(vec![usage(41, Some(100))]);
        let processes = FakeProcessTable::new([(1, 0, 1)]);

        let error = observe(&source, &processes).unwrap_err();

        assert_eq!(error.to_string(), "PID 41 is not running");
    }

    #[test]
    fn rejects_process_attributed_to_multiple_devices() {
        let source = FakeNvidia::new(vec![
            vec![vec![usage(41, Some(10))]],
            vec![vec![usage(1275, Some(20))]],
        ]);

        let error = observe(&source, &workload_table()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "PID 41 is ambiguously attributed to NVIDIA devices [0, 1]"
        );
    }

    #[test]
    fn rejects_unavailable_process_memory() {
        let source = FakeNvidia::single(vec![usage(41, Some(100)), usage(1275, None)]);

        let error = observe(&source, &workload_table()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "NVML memory is unavailable for PID 1275 on NVIDIA device 0"
        );
    }

    #[test]
    fn rejects_process_becoming_ambiguous_during_sampling() {
        let source = FakeNvidia::new(vec![
            vec![vec![usage(41, Some(100))]],
            vec![Vec::new(), vec![usage(41, Some(200))]],
        ]);

        let error = observe(&source, &workload_table()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "PID 41 is ambiguously attributed to NVIDIA devices [0, 1]"
        );
    }

    #[test]
    fn reports_other_processes_without_counting_them_for_the_workload() {
        let shared = vec![
            usage(41, Some(100)),
            usage(1275, Some(50)),
            usage(99, Some(200)),
            usage(98, None),
        ];
        let source = FakeNvidia::new(vec![vec![
            vec![usage(41, Some(100)), usage(1275, Some(50))],
            shared.clone(),
            shared,
        ]]);
        let processes = workload_table();
        processes.set(98, Some((1, 760)));

        let observation = observe(&source, &processes).unwrap();

        assert!(!observation.isolated);
        assert_eq!(observation.samples[0].used_bytes, 150);
        assert_eq!(observation.samples[0].unattributed_bytes, 200);
        assert_eq!(observation.observed_max_bytes, 150);
        assert_eq!(observation.unattributed_max_bytes, 200);
        assert_eq!(
            process_lifetimes(&observation),
            vec![(41, 700), (1275, 800)]
        );
        assert_eq!(
            observation.other_processes,
            vec![
                VramOtherProcess {
                    process_id: 98,
                    memory_known: false,
                },
                VramOtherProcess {
                    process_id: 99,
                    memory_known: true,
                },
            ]
        );
    }

    #[test]
    fn drops_a_process_that_exited_before_its_ancestry_was_read() {
        let source = FakeNvidia::single(vec![usage(41, Some(100)), usage(1300, Some(500))]);

        let observation = observe(&source, &workload_table()).unwrap();

        assert_eq!(observation.samples[0].used_bytes, 100);
        assert_eq!(process_lifetimes(&observation), vec![(41, 700)]);
    }

    #[test]
    fn drops_a_process_whose_pid_was_reused_during_the_ancestry_walk() {
        // The reused PID belongs to a new process that never uses the GPU, so NVML stops listing it.
        let source = FakeNvidia::new(vec![vec![
            vec![usage(41, Some(100)), usage(1275, Some(200))],
            vec![usage(41, Some(100))],
        ]]);
        let processes = ReusedPidTable {
            table: workload_table(),
            process_id: 1275,
            reads: Mutex::new(0),
        };

        let observation = observe(&source, &processes).unwrap();

        assert_eq!(observation.samples[0].used_bytes, 100);
        assert_eq!(process_lifetimes(&observation), vec![(41, 700)]);
    }

    #[test]
    fn keeps_every_child_lifetime_seen_when_a_child_restarts() {
        let source = FakeNvidia::new(vec![vec![
            vec![usage(41, Some(100)), usage(1275, Some(200))],
            vec![usage(41, Some(100)), usage(1275, Some(200))],
            vec![usage(41, Some(100)), usage(1290, Some(300))],
        ]]);
        let processes = workload_table();
        let mut restart_child = |_: &VramSample| {
            processes.set(1275, None);
            processes.set(1290, Some((41, 900)));
            Ok(())
        };

        let observation = sample_process_from(
            &source,
            &processes,
            41,
            None,
            short_window(),
            &VramCancellation::default(),
            &mut restart_child,
        )
        .unwrap();

        assert_eq!(
            process_lifetimes(&observation),
            vec![(41, 700), (1275, 800), (1290, 900)]
        );
    }

    #[test]
    fn rejects_process_identity_change() {
        let source = FakeNvidia::single(vec![usage(41, Some(100))]);
        let processes = workload_table();
        let mut replace_root = |_: &VramSample| {
            processes.set(41, Some((1, 701)));
            Ok(())
        };

        let error = sample_process_from(
            &source,
            &processes,
            41,
            None,
            short_window(),
            &VramCancellation::default(),
            &mut replace_root,
        )
        .unwrap_err();

        assert_eq!(
            error.to_string(),
            "process identity changed while sampling PID 41"
        );
    }

    #[test]
    fn child_accounting_failure_is_unavailable_evidence() {
        let mut source = FakeNvidia::single(vec![usage(41, Some(100)), usage(1275, Some(200))]);
        source.failing_accounting = BTreeSet::from([1275]);

        let observation = observe(&source, &workload_table()).unwrap();

        let reasons = observation
            .processes
            .iter()
            .map(|process| match &process.accounting {
                VramAvailability::Unavailable { reason } => (process.process_id, reason.as_str()),
                VramAvailability::Available { .. } => (process.process_id, "available"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            reasons,
            vec![
                (41, "fixture accounting disabled"),
                (1275, "fixture accounting identity mismatch")
            ]
        );
    }

    #[test]
    fn root_accounting_failure_fails_the_observation() {
        let mut source = FakeNvidia::single(vec![usage(41, Some(100))]);
        source.failing_accounting = BTreeSet::from([41]);

        let error = observe(&source, &workload_table()).unwrap_err();

        assert_eq!(error.to_string(), "fixture accounting identity mismatch");
    }

    #[test]
    fn honors_preexisting_cancellation() {
        let source = FakeNvidia::single(vec![usage(41, Some(100))]);
        let cancellation = VramCancellation::default();
        cancellation.cancel();

        let error = sample_process_from(
            &source,
            &workload_table(),
            41,
            None,
            SamplingWindow {
                duration: Duration::from_millis(1),
                interval: Duration::from_millis(20),
            },
            &cancellation,
            &mut |_| Ok(()),
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "VRAM observation was cancelled");
    }

    #[test]
    fn honors_cancellation_during_sampling() {
        let source = FakeNvidia::single(vec![usage(41, Some(100))]);
        let cancellation = VramCancellation::default();
        let mut observed_samples = 0;
        let mut observer = |_: &VramSample| {
            observed_samples += 1;
            cancellation.cancel();
            Ok(())
        };

        let error = sample_process_from(
            &source,
            &workload_table(),
            41,
            None,
            SamplingWindow {
                duration: Duration::from_millis(100),
                interval: Duration::from_millis(20),
            },
            &cancellation,
            &mut observer,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "VRAM observation was cancelled");
        assert_eq!(observed_samples, 1);
    }

    #[test]
    fn rejects_sampling_inputs_outside_bounds() {
        let source = FakeNvidia::single(vec![usage(41, Some(100))]);
        let invalid_windows = [
            (
                SamplingWindow {
                    duration: Duration::ZERO,
                    interval: Duration::from_millis(20),
                },
                "duration must be greater than zero",
            ),
            (
                SamplingWindow {
                    duration: Duration::from_millis(3_600_001),
                    interval: Duration::from_millis(20),
                },
                "duration must not exceed 3600000 milliseconds",
            ),
            (
                SamplingWindow {
                    duration: Duration::from_millis(1),
                    interval: Duration::from_millis(19),
                },
                "interval must be between 20 and 50 milliseconds",
            ),
            (
                SamplingWindow {
                    duration: Duration::from_millis(1),
                    interval: Duration::from_millis(51),
                },
                "interval must be between 20 and 50 milliseconds",
            ),
        ];

        for (window, expected) in invalid_windows {
            let error = sample_process_from(
                &source,
                &workload_table(),
                41,
                None,
                window,
                &VramCancellation::default(),
                &mut |_| Ok(()),
            )
            .unwrap_err();

            assert_eq!(error.to_string(), expected);
        }
    }

    fn observe(
        source: &impl NvidiaSource,
        processes: &dyn ProcessTable,
    ) -> Result<VramObservation> {
        sample_process_from(
            source,
            processes,
            41,
            None,
            short_window(),
            &VramCancellation::default(),
            &mut |_| Ok(()),
        )
    }

    fn short_window() -> SamplingWindow {
        SamplingWindow {
            duration: Duration::from_millis(1),
            interval: Duration::from_millis(20),
        }
    }

    /// Root 41 with children 1275 and 1276, and unrelated process 99, all under init.
    fn workload_table() -> FakeProcessTable {
        FakeProcessTable::new([
            (1, 0, 1),
            (41, 1, 700),
            (1275, 41, 800),
            (1276, 1275, 850),
            (99, 1, 750),
        ])
    }

    fn process_lifetimes(observation: &VramObservation) -> Vec<(u32, u64)> {
        observation
            .processes
            .iter()
            .map(|process| (process.process_id, process.start_token))
            .collect()
    }

    fn usage(process_id: u32, used_bytes: Option<u64>) -> ProcessUsage {
        ProcessUsage {
            process_id,
            used_bytes,
        }
    }

    /// Reports one PID as a workload child on its first read, then as an unrelated later process.
    struct ReusedPidTable {
        table: FakeProcessTable,
        process_id: u32,
        reads: Mutex<usize>,
    }

    impl ProcessTable for ReusedPidTable {
        fn entry(&self, process_id: u32) -> Result<Option<ProcessEntry>> {
            if process_id != self.process_id {
                return self.table.entry(process_id);
            }
            let mut reads = self.reads.lock().unwrap();
            *reads += 1;
            Ok(Some(ProcessEntry {
                parent_process_id: if *reads == 1 { 41 } else { 1 },
                start_token: if *reads == 1 { 800 } else { 950 },
            }))
        }
    }

    struct FakeNvidia {
        devices: Vec<Mutex<VecDeque<Vec<ProcessUsage>>>>,
        failing_accounting: BTreeSet<u32>,
    }

    impl FakeNvidia {
        fn single(processes: Vec<ProcessUsage>) -> Self {
            Self::new(vec![vec![processes.clone(), processes.clone(), processes]])
        }

        fn new(devices: Vec<Vec<Vec<ProcessUsage>>>) -> Self {
            Self {
                devices: devices
                    .into_iter()
                    .map(|samples| Mutex::new(VecDeque::from(samples)))
                    .collect(),
                failing_accounting: BTreeSet::new(),
            }
        }
    }

    impl NvidiaSource for FakeNvidia {
        fn device_indices(&self) -> Result<Vec<u32>> {
            Ok((0..self.devices.len() as u32).collect())
        }

        fn device_identity(&self, index: u32) -> Result<VramGpuIdentity> {
            Ok(VramGpuIdentity {
                index,
                uuid: format!("GPU-fixture-{index}"),
                name: format!("Fixture GPU {index}"),
                driver_version: "fixture-driver".into(),
                cuda_driver_version: 13_000,
                memory: VramGpuMemory::Dedicated {
                    total_bytes: 1_000_000,
                },
            })
        }

        fn process_usage(&self, index: u32) -> Result<Vec<ProcessUsage>> {
            let mut samples = self.devices[index as usize].lock().unwrap();
            match samples.len() {
                0 => bail!("no process samples"),
                1 => Ok(samples.front().unwrap().clone()),
                _ => Ok(samples.pop_front().unwrap()),
            }
        }

        fn accounting(
            &self,
            _index: u32,
            process_id: u32,
            _process_start_token: u64,
        ) -> Result<VramAvailability<VramAccounting>> {
            if self.failing_accounting.contains(&process_id) {
                bail!("fixture accounting identity mismatch");
            }
            Ok(VramAvailability::Unavailable {
                reason: "fixture accounting disabled".into(),
            })
        }
    }
}
