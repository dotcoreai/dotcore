//! Observes the Apple Silicon GPU of a macOS node through Metal for the node hardware snapshot.
//!
//! The GPU is reported with its Metal name, unified memory, and the working-set limit Metal
//! recommends, under the fixed device id `apple-gpu-0`. Its memory pool is the host memory that host
//! facts describe, and no per-process rows are reported: deployment memory on Apple nodes is
//! measured on the workloads Dotcore owns, never from node-wide GPU client lists.

use dotcore_domain::APPLE_GPU_DEVICE_ID;
use dotcore_proto::dotcore::observation::v1::{
    AcceleratorFacts, AcceleratorMemoryArchitecture, AcceleratorVendor,
};
use tracing::info;

use crate::hardware::{
    AcceleratorDiscovery, AcceleratorUnavailable, AcceleratorVendorAdapter,
    AcceleratorVendorDiscoverer, AcceleratorVendorObservation, AcceleratorVendorPoller,
    UnavailableReason,
};

/// Discovers the default Metal GPU once and polls its facts for the node snapshot.
pub struct SystemAppleAcceleratorAdapter {
    source: Box<dyn AppleGpuSource>,
}

impl SystemAppleAcceleratorAdapter {
    /// Creates an adapter that opens the host's default Metal device on discovery.
    pub fn for_current_host() -> Self {
        Self::with_source(Box::new(MetalGpuSource))
    }

    fn with_source(source: Box<dyn AppleGpuSource>) -> Self {
        Self { source }
    }

    fn discover_apple(&self) -> Result<AppleAcceleratorPoller, UnavailableReason> {
        let (gpu, facts) = self.open_unified_gpu()?;
        info!(
            device_id = APPLE_GPU_DEVICE_ID,
            model = %facts.name,
            working_set_limit_bytes = facts.working_set_limit_bytes,
            "discovered Apple GPU"
        );
        Ok(AppleAcceleratorPoller { gpu })
    }

    /// Opens the default GPU without logging, so one-shot command probes stay silent.
    fn open_unified_gpu(&self) -> Result<(Box<dyn AppleGpu>, AppleGpuFacts), UnavailableReason> {
        let gpu = self
            .source
            .open_default()
            .ok_or(UnavailableReason::NoDevice)?;
        let facts = gpu.facts();
        // Only Apple Silicon GPUs share host memory; a discrete Mac GPU is not Apple's.
        if !facts.unified_memory {
            return Err(UnavailableReason::Unsupported);
        }
        Ok((gpu, facts))
    }
}

impl AcceleratorVendorDiscoverer for SystemAppleAcceleratorAdapter {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Apple
    }

    fn discover(&self) -> AcceleratorDiscovery {
        match self.discover_apple() {
            Ok(poller) => AcceleratorDiscovery::Detected(Box::new(poller)),
            Err(reason) => AcceleratorDiscovery::Unavailable(reason),
        }
    }
}

impl AcceleratorVendorAdapter for SystemAppleAcceleratorAdapter {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Apple
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        let (gpu, _) = self
            .open_unified_gpu()
            .map_err(AcceleratorUnavailable::stable)?;
        let observation = AppleAcceleratorPoller { gpu }
            .poll()
            .map_err(AcceleratorUnavailable::stable)?;
        Ok(observation.facts)
    }
}

/// Opens the default GPU; the seam that keeps discovery testable without Metal.
trait AppleGpuSource: Send + Sync {
    /// Returns the default GPU, or `None` when Metal reports no device.
    fn open_default(&self) -> Option<Box<dyn AppleGpu>>;
}

/// One opened GPU whose facts are read on every poll.
trait AppleGpu: Send {
    fn facts(&self) -> AppleGpuFacts;
}

/// Facts Metal reports for one GPU.
#[derive(Clone, Debug, PartialEq, Eq)]
struct AppleGpuFacts {
    name: String,
    unified_memory: bool,
    working_set_limit_bytes: u64,
}

struct AppleAcceleratorPoller {
    gpu: Box<dyn AppleGpu>,
}

impl AcceleratorVendorPoller for AppleAcceleratorPoller {
    fn poll(&self) -> Result<AcceleratorVendorObservation, UnavailableReason> {
        let facts = self.gpu.facts();
        Ok(AcceleratorVendorObservation {
            facts: vec![AcceleratorFacts {
                vendor: AcceleratorVendor::Apple as i32,
                model: facts.name,
                memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
                // Host facts describe the unified pool; the GPU reports no memory of its own.
                total_memory_bytes: None,
                driver_or_runtime_version: String::new(),
                device_id: APPLE_GPU_DEVICE_ID.to_owned(),
                // Macs have one GPU, so its enumeration index is always zero.
                vendor_index: Some(0),
                used_memory_bytes: None,
                working_set_limit_bytes: Some(facts.working_set_limit_bytes),
                // Utilization, temperature and power are readable only through private Apple
                // interfaces, so none is supported.
                supported_signals: Vec::new(),
            }],
            processes: Vec::new(),
        })
    }
}

/// Reads the system default device through the Metal framework.
struct MetalGpuSource;

impl AppleGpuSource for MetalGpuSource {
    fn open_default(&self) -> Option<Box<dyn AppleGpu>> {
        let device = objc2_metal::MTLCreateSystemDefaultDevice()?;
        Some(Box::new(MetalGpu { device }))
    }
}

struct MetalGpu {
    device: objc2::rc::Retained<objc2::runtime::ProtocolObject<dyn objc2_metal::MTLDevice>>,
}

impl AppleGpu for MetalGpu {
    fn facts(&self) -> AppleGpuFacts {
        use objc2_metal::MTLDevice as _;

        // Collection runs on pool threads with no autorelease pool, so drain returned objects.
        objc2::rc::autoreleasepool(|_| AppleGpuFacts {
            name: self.device.name().to_string(),
            unified_memory: self.device.hasUnifiedMemory(),
            working_set_limit_bytes: self.device.recommendedMaxWorkingSetSize(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use dotcore_domain::{
        clock::{Clock, ClockSleep},
        hardware_observation::{
            AcceleratorProcessObservation, AcceleratorVendorProcessObservation,
            AcceleratorVendorProcessResult, HardwareSnapshotSource,
        },
    };
    use dotcore_proto::dotcore::observation::v1::{
        AcceleratorObservation, HostFacts, ObservationUnavailable, accelerator_observation,
    };

    use super::*;
    use crate::hardware::{HardwareSnapshotCollector, HostFactsAdapter};

    const HOST_TOTAL: u64 = 38_654_705_664;

    #[test]
    fn apple_gpu_reports_unified_facts_under_a_fixed_device_id_without_process_rows() {
        let working_set = Arc::new(AtomicU64::new(28_991_029_248));
        let collector = collector(FakeGpuSource::gpu(true, Arc::clone(&working_set)));

        let first = collector.collect_observation();
        working_set.store(30_000_000_000, Ordering::SeqCst);
        let second = collector.collect_observation();

        assert_eq!(
            first.snapshot.accelerators,
            [apple_facts_observation(28_991_029_248)]
        );
        assert_eq!(
            second.snapshot.accelerators,
            [apple_facts_observation(30_000_000_000)]
        );
        assert_eq!(
            second.accelerator_processes,
            AcceleratorProcessObservation::Observed(vec![AcceleratorVendorProcessObservation {
                vendor: AcceleratorVendor::Apple,
                result: AcceleratorVendorProcessResult::Observed(Vec::new()),
            }])
        );
    }

    #[test]
    fn probe_adapter_reports_the_fixed_device_or_the_unavailable_reason() {
        let gpu = SystemAppleAcceleratorAdapter::with_source(Box::new(FakeGpuSource::gpu(
            true,
            Arc::new(AtomicU64::new(28_991_029_248)),
        )));
        let facts = gpu.collect_accelerator_facts().expect("facts");
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].device_id, "apple-gpu-0");

        for (source, reason) in [
            (FakeGpuSource::absent(), UnavailableReason::NoDevice),
            (
                FakeGpuSource::gpu(false, Arc::new(AtomicU64::new(1))),
                UnavailableReason::Unsupported,
            ),
        ] {
            let error = SystemAppleAcceleratorAdapter::with_source(Box::new(source))
                .collect_accelerator_facts()
                .expect_err("unavailable");
            assert_eq!(error.reason(), reason);
        }
    }

    #[test]
    fn missing_or_discrete_gpus_report_an_explicit_unavailable_observation() {
        for (source, code, message) in [
            (
                FakeGpuSource::absent(),
                "NO_DEVICE",
                "Apple observation is unavailable because no device is present.",
            ),
            (
                FakeGpuSource::gpu(false, Arc::new(AtomicU64::new(1))),
                "UNSUPPORTED",
                "Apple observation is unavailable because this host does not support it.",
            ),
        ] {
            let unavailable = ObservationUnavailable {
                code: code.to_owned(),
                message: message.to_owned(),
            };

            let observation = collector(source).collect_observation();

            assert_eq!(
                observation.snapshot.accelerators,
                [AcceleratorObservation {
                    vendor: AcceleratorVendor::Apple as i32,
                    result: Some(accelerator_observation::Result::Unavailable(
                        unavailable.clone()
                    )),
                }]
            );
            assert_eq!(
                observation.accelerator_processes,
                AcceleratorProcessObservation::Observed(vec![
                    AcceleratorVendorProcessObservation {
                        vendor: AcceleratorVendor::Apple,
                        result: AcceleratorVendorProcessResult::Unavailable(unavailable),
                    }
                ])
            );
        }
    }

    fn collector(source: FakeGpuSource) -> HardwareSnapshotCollector<FakeHost> {
        HardwareSnapshotCollector::new(
            FakeHost,
            vec![Box::new(SystemAppleAcceleratorAdapter::with_source(
                Box::new(source),
            ))],
            Arc::new(FakeClock),
        )
    }

    fn apple_facts_observation(working_set_limit_bytes: u64) -> AcceleratorObservation {
        AcceleratorObservation {
            vendor: AcceleratorVendor::Apple as i32,
            result: Some(accelerator_observation::Result::Facts(
                dotcore_proto::dotcore::observation::v1::AcceleratorFactsSet {
                    facts: vec![AcceleratorFacts {
                        vendor: AcceleratorVendor::Apple as i32,
                        model: "Apple M3 Max".to_owned(),
                        memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
                        total_memory_bytes: None,
                        driver_or_runtime_version: String::new(),
                        device_id: "apple-gpu-0".to_owned(),
                        vendor_index: Some(0),
                        used_memory_bytes: None,
                        working_set_limit_bytes: Some(working_set_limit_bytes),
                        supported_signals: Vec::new(),
                    }],
                },
            )),
        }
    }

    struct FakeGpuSource(Option<FakeGpu>);

    impl FakeGpuSource {
        fn gpu(unified_memory: bool, working_set_limit_bytes: Arc<AtomicU64>) -> Self {
            Self(Some(FakeGpu {
                unified_memory,
                working_set_limit_bytes,
            }))
        }

        fn absent() -> Self {
            Self(None)
        }
    }

    impl AppleGpuSource for FakeGpuSource {
        fn open_default(&self) -> Option<Box<dyn AppleGpu>> {
            self.0
                .as_ref()
                .map(|gpu| Box::new(gpu.clone()) as Box<dyn AppleGpu>)
        }
    }

    #[derive(Clone)]
    struct FakeGpu {
        unified_memory: bool,
        working_set_limit_bytes: Arc<AtomicU64>,
    }

    impl AppleGpu for FakeGpu {
        fn facts(&self) -> AppleGpuFacts {
            AppleGpuFacts {
                name: "Apple M3 Max".to_owned(),
                unified_memory: self.unified_memory,
                working_set_limit_bytes: self.working_set_limit_bytes.load(Ordering::SeqCst),
            }
        }
    }

    struct FakeHost;

    impl HostFactsAdapter for FakeHost {
        fn collect_host_facts(&self) -> Result<HostFacts, UnavailableReason> {
            Ok(HostFacts {
                host_name: "mac".to_owned(),
                operating_system_or_kernel: "macOS 26".to_owned(),
                cpu_count: 16,
                total_memory_bytes: HOST_TOTAL,
                available_memory_bytes: HOST_TOTAL / 2,
                cached_memory_bytes: None,
                filesystems: Vec::new(),
            })
        }
    }

    /// Wall time fixed at 10 s after the Unix epoch.
    #[derive(Debug)]
    struct FakeClock;

    impl Clock for FakeClock {
        fn now(&self) -> tokio::time::Instant {
            tokio::time::Instant::now()
        }

        fn wall_now(&self) -> std::time::SystemTime {
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(10)
        }

        fn sleep_until(&self, deadline: tokio::time::Instant) -> ClockSleep {
            Box::pin(tokio::time::sleep_until(deadline))
        }
    }
}
