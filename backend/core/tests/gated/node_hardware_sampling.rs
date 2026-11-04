//! Measures the per-second hardware sample on real hosts: host memory, CPU and filesystem space
//! everywhere, plus NVIDIA device memory, utilization, temperature and power on Linux. Apple
//! Silicon GPUs allocate from host memory and report no per-second signal. It also checks that a
//! real host's readings equal its machine's description in `machines/`, for dedicated GPU desktops
//! and for whole machines whose GPUs share host memory.
//!
//! A node takes this sample every second, so its p99 time must stay well under 100 ms; each test
//! prints p50, p99 and the maximum for the qualification record, and the filesystems it read.

use std::{
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use dotcore_domain::{
    clock::SystemClock,
    hardware_observation::{HardwareSample, HardwareSnapshotSource},
};
use dotcore_node_hardware::{MachineDescription, hardware::SystemHardwareSnapshotCollector};
use dotcore_proto::dotcore::observation::v1::{
    AcceleratorFacts, AcceleratorVendor, HardwareSnapshot, accelerator_observation,
    host_observation,
};

#[cfg(target_os = "linux")]
#[test]
#[ignore = "hardware: just backend::test-nvidia"]
fn hw_nvidia_sample_records_device_signals_with_p99_well_under_100_ms() {
    use dotcore_proto::dotcore::observation::v1::AcceleratorSignal;

    let collector =
        SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock));
    // Accelerators are discovered by the full snapshot, as on a node before its first sample.
    let snapshot = collector.collect_snapshot();
    let devices = vendor_facts(&snapshot, AcceleratorVendor::Nvidia);
    assert!(!devices.is_empty(), "an NVIDIA device is discovered");
    for device in &devices {
        assert_eq!(
            device.supported_signals,
            [
                AcceleratorSignal::Utilization as i32,
                AcceleratorSignal::Temperature as i32,
                AcceleratorSignal::Power as i32,
            ],
            "{} supports every signal",
            device.device_id
        );
    }

    let samples = sample_with_p99_well_under_100_ms(&collector, &snapshot);

    let last = samples.last().expect("samples");
    assert_eq!(
        last.accelerators
            .iter()
            .map(|device| device.device_id.as_str())
            .collect::<Vec<_>>(),
        devices
            .iter()
            .map(|device| device.device_id.as_str())
            .collect::<Vec<_>>()
    );
    for device in &last.accelerators {
        eprintln!(
            "device {}: utilization {:?}, temperature {:?} °C, power {:?} W",
            device.device_id, device.utilization, device.temperature_celsius, device.power_watts
        );
        let utilization = device.utilization.expect("utilization is sampled");
        assert!((0.0..=1.0).contains(&utilization), "{utilization}");
        let temperature = device.temperature_celsius.expect("temperature is sampled");
        assert!((1.0..=120.0).contains(&temperature), "{temperature}");
        let power = device.power_watts.expect("power is sampled");
        assert!((1.0..=2_000.0).contains(&power), "{power}");
    }
}

/// Idle GPU use may differ from the described idle use by this much: the reading also counts the
/// display and compositor, which come and go.
#[cfg(target_os = "linux")]
const IDLE_GPU_USE_MARGIN_BYTES: u64 = 64 * 1024 * 1024;

/// A host reads at most this share less than its installed memory: the kernel and firmware keep
/// the rest (62.0 of 64 GiB on the reference desktop).
#[cfg(target_os = "linux")]
const HOST_TOTAL_RESERVED_SHARE: f64 = 0.04;

/// The reading of an idle RTX 5090 desktop equals the `rtx-5090-desktop` description: vendor,
/// model, architecture and GPU total exactly, the host total within the share the system keeps
/// from the installed memory, and idle GPU use within a margin. The described host total is the
/// installed memory, which varies by board, so it isn't compared exactly. The driver-reserved
/// memory is judged only through idle use, the one reading that separates it (no GPU process); the
/// driver version, device ID and CPU count belong to the install and the CPU.
#[cfg(target_os = "linux")]
#[test]
#[ignore = "hardware: just backend::test-nvidia-5090"]
fn hw_nvidia5090_readings_match_the_rtx_5090_desktop_description() {
    use crate::nvidia_qualification::GpuObservation;

    let collector =
        SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock));
    let observation = collector.collect_observation();
    let read = observation.snapshot;
    let named = real_machine("rtx-5090-desktop")
        .render(&[], SystemTime::now())
        .expect("render the machine description");

    let read_gpus = vendor_facts(&read, AcceleratorVendor::Nvidia);
    let named_gpus = vendor_facts(&named, AcceleratorVendor::Nvidia);
    assert_eq!(read_gpus.len(), named_gpus.len(), "GPU count");
    let (read_gpu, named_gpu) = (&read_gpus[0], &named_gpus[0]);
    eprintln!("GPU read {read_gpu:?}");
    eprintln!("GPU named {named_gpu:?}");
    assert_eq!(read_gpu.vendor, named_gpu.vendor, "vendor");
    assert_eq!(read_gpu.model, named_gpu.model, "model");
    assert_eq!(
        read_gpu.memory_architecture, named_gpu.memory_architecture,
        "memory architecture"
    );
    assert_eq!(
        read_gpu.total_memory_bytes, named_gpu.total_memory_bytes,
        "GPU total"
    );

    let host_facts = |snapshot: &HardwareSnapshot| match snapshot
        .host
        .as_ref()
        .and_then(|host| host.result.clone())
    {
        Some(host_observation::Result::Facts(host)) => host,
        other => panic!("host facts are observed: {other:?}"),
    };
    let (read_host, named_host) = (host_facts(&read), host_facts(&named));
    let (read_total, named_total) = (read_host.total_memory_bytes, named_host.total_memory_bytes);
    eprintln!(
        "host read: total {read_total} B, available {} B; named total {named_total} B",
        read_host.available_memory_bytes
    );
    assert!(
        read_total <= named_total
            && (named_total - read_total) as f64 <= named_total as f64 * HOST_TOTAL_RESERVED_SHARE,
        "host total read {read_total} B vs installed {named_total} B (reserved share {HOST_TOTAL_RESERVED_SHARE})"
    );

    // A GPU process adds its own memory to the use, so idle use is comparable only without one.
    let gpu = GpuObservation::from_hardware(&collector.collect_observation())
        .expect("observe the GPU processes");
    assert_eq!(gpu.processes, [], "the GPU runs no process");
    let read_used = read_gpu.used_memory_bytes.expect("GPU use is read");
    let named_used = named_gpu.used_memory_bytes.expect("GPU use is named");
    eprintln!("idle GPU use read {read_used} B, named {named_used} B");
    assert!(
        read_used.abs_diff(named_used) <= IDLE_GPU_USE_MARGIN_BYTES,
        "idle GPU use read {read_used} B vs named {named_used} B (margin {IDLE_GPU_USE_MARGIN_BYTES} B)"
    );
}

/// The reading of a DGX Spark equals the `gb10` description (see
/// [`assert_unified_machine_readings`]).
#[cfg(target_os = "linux")]
#[test]
#[ignore = "hardware: just backend::test-gb10"]
fn hw_gb10_readings_match_the_gb10_description() {
    assert_unified_machine_readings("gb10", AcceleratorVendor::Nvidia);
}

/// The reading of the reference Apple Silicon Mac equals the `mac-m3-max-36g` description (see
/// [`assert_unified_machine_readings`]).
#[cfg(target_os = "macos")]
#[test]
#[ignore = "hardware: just backend::test-apple"]
fn hw_apple_readings_match_the_mac_m3_max_36g_description() {
    assert_unified_machine_readings("mac-m3-max-36g", AcceleratorVendor::Apple);
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "hardware: just backend::test-apple"]
fn hw_apple_sample_records_host_signals_and_no_gpu_signal_with_p99_well_under_100_ms() {
    let collector =
        SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock));
    let snapshot = collector.collect_snapshot();
    let devices = vendor_facts(&snapshot, AcceleratorVendor::Apple);
    assert_eq!(devices.len(), 1, "the Apple GPU is discovered");
    assert_eq!(devices[0].supported_signals, Vec::<i32>::new());

    let samples = sample_with_p99_well_under_100_ms(&collector, &snapshot);

    assert!(
        samples.iter().all(|sample| sample.accelerators.is_empty()),
        "the Apple GPU has no per-second signal"
    );
}

/// Host memory in use may differ from a unified machine description's idle use by this share of the
/// total: desktop sessions, caches and background services come and go.
const UNIFIED_IDLE_USE_MARGIN_SHARE: f64 = 0.10;

/// Checks that this host's reading equals the unified machine description `name`, a whole machine whose
/// GPUs share host memory: CPU count, host total, GPU count, vendor, model, memory architecture and
/// working-set limit exactly, and host memory in use within [`UNIFIED_IDLE_USE_MARGIN_SHARE`] of
/// the described idle use. Unified GPUs report no memory of their own on either side.
fn assert_unified_machine_readings(name: &str, vendor: AcceleratorVendor) {
    let collector =
        SystemHardwareSnapshotCollector::with_platform_accelerators(Arc::new(SystemClock));
    let read = collector.collect_snapshot();
    let named = real_machine(name)
        .render(&[], SystemTime::now())
        .expect("render the machine description");

    let host_facts = |snapshot: &HardwareSnapshot| match snapshot
        .host
        .as_ref()
        .and_then(|host| host.result.clone())
    {
        Some(host_observation::Result::Facts(host)) => host,
        other => panic!("host facts are observed: {other:?}"),
    };
    let (read_host, named_host) = (host_facts(&read), host_facts(&named));
    let read_used = read_host.total_memory_bytes - read_host.available_memory_bytes;
    let named_used = named_host.total_memory_bytes - named_host.available_memory_bytes;
    eprintln!(
        "host read: {} CPUs, total {} B, used {read_used} B, cached {:?} B; named: {} CPUs, total {} B, idle used {named_used} B",
        read_host.cpu_count,
        read_host.total_memory_bytes,
        read_host.cached_memory_bytes,
        named_host.cpu_count,
        named_host.total_memory_bytes,
    );

    let read_gpus = vendor_facts(&read, vendor);
    let named_gpus = vendor_facts(&named, vendor);
    for gpu in &read_gpus {
        eprintln!("GPU read {gpu:?}");
    }
    for gpu in &named_gpus {
        eprintln!("GPU named {gpu:?}");
    }

    assert_eq!(read_host.cpu_count, named_host.cpu_count, "CPU count");
    assert_eq!(
        read_host.total_memory_bytes, named_host.total_memory_bytes,
        "host total"
    );
    let summary = |gpus: &[AcceleratorFacts]| {
        gpus.iter()
            .map(|gpu| {
                (
                    gpu.vendor,
                    gpu.model.clone(),
                    gpu.memory_architecture,
                    gpu.total_memory_bytes,
                    gpu.working_set_limit_bytes,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(summary(&read_gpus), summary(&named_gpus), "GPUs");
    let margin = named_host.total_memory_bytes as f64 * UNIFIED_IDLE_USE_MARGIN_SHARE;
    assert!(
        read_used.abs_diff(named_used) as f64 <= margin,
        "host use read {read_used} B vs named idle {named_used} B (margin {margin} B)"
    );
}

/// The real machines these tests compare readings with, by name, as YAML descriptions. They are
/// test data, apart from the hardware simulated inference offers: they follow the latest reading of
/// the reference machine of each kind.
const REAL_MACHINES: &[(&str, &str)] = &[
    ("gb10", include_str!("machines/gb10.yaml")),
    (
        "mac-m3-max-36g",
        include_str!("machines/mac-m3-max-36g.yaml"),
    ),
    (
        "rtx-5090-desktop",
        include_str!("machines/rtx-5090-desktop.yaml"),
    ),
];

/// Returns the real machine description `name` of [`REAL_MACHINES`].
fn real_machine(name: &str) -> MachineDescription {
    let (_, yaml) = REAL_MACHINES
        .iter()
        .find(|(known, _)| *known == name)
        .unwrap_or_else(|| panic!("no real machine description {name:?}"));
    MachineDescription::from_yaml(name, yaml).expect("real machine description")
}

/// Samples taken to measure the latency distribution.
const SAMPLES: usize = 1_000;

/// Times [`SAMPLES`] samples, prints their p50, p99 and maximum and the filesystems read, and
/// asserts the p99 stays under 100 ms. Every sample reads host memory and the snapshot's
/// filesystems, and every one after the first reads CPU utilization.
fn sample_with_p99_well_under_100_ms(
    collector: &SystemHardwareSnapshotCollector,
    snapshot: &HardwareSnapshot,
) -> Vec<HardwareSample> {
    let Some(host_observation::Result::Facts(host)) =
        snapshot.host.as_ref().and_then(|host| host.result.clone())
    else {
        panic!("host facts are observed");
    };
    assert!(!host.filesystems.is_empty(), "a real filesystem is listed");
    for filesystem in &host.filesystems {
        eprintln!(
            "filesystem {} ({}): {} bytes",
            filesystem.mountpoint, filesystem.fstype, filesystem.size_bytes
        );
    }

    let mut durations = Vec::with_capacity(SAMPLES);
    let mut samples = Vec::with_capacity(SAMPLES);
    for index in 0..SAMPLES {
        let started = Instant::now();
        let sample = collector.sample();
        durations.push(started.elapsed());

        assert!(sample.host.memory.is_some(), "host memory is sampled");
        assert_eq!(sample.host.cpu_utilization.is_some(), index > 0, "{index}");
        assert_eq!(
            sample
                .host
                .filesystems
                .iter()
                .map(|filesystem| (filesystem.mountpoint.as_str(), filesystem.fstype.as_str()))
                .collect::<Vec<_>>(),
            host.filesystems
                .iter()
                .map(|filesystem| (filesystem.mountpoint.as_str(), filesystem.fstype.as_str()))
                .collect::<Vec<_>>()
        );
        for filesystem in &sample.host.filesystems {
            assert!(
                filesystem.used_bytes + filesystem.available_bytes <= filesystem.size_bytes,
                "{filesystem:?}"
            );
        }
        samples.push(sample);
    }
    durations.sort();
    let p50 = durations[SAMPLES / 2];
    let p99 = durations[SAMPLES * 99 / 100];
    let max = durations[SAMPLES - 1];
    eprintln!("hardware sample over {SAMPLES} runs: p50 {p50:?}, p99 {p99:?}, max {max:?}");

    assert!(p99 < Duration::from_millis(100), "p99 {p99:?}");
    samples
}

/// Device facts the snapshot reports for `vendor`.
fn vendor_facts(snapshot: &HardwareSnapshot, vendor: AcceleratorVendor) -> Vec<AcceleratorFacts> {
    snapshot
        .accelerators
        .iter()
        .filter(|observation| observation.vendor == vendor as i32)
        .find_map(|observation| match &observation.result {
            Some(accelerator_observation::Result::Facts(facts)) => Some(facts.facts.clone()),
            _ => None,
        })
        .unwrap_or_default()
}
