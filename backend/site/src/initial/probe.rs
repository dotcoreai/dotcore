//! Probes the local host once, read-only, to report which capabilities the new site can use.
//!
//! An injected Docker capability probe and a local NVIDIA accelerator adapter are asked what
//! is available, and their facts are returned as a single probe report. Probing never changes
//! host state; the report feeds initial site creation. A machine description gives the report a
//! host of that hardware would, so a machine can be planned without probing it.

use std::collections::BTreeSet;

use dotcore_domain::{ExecutionPlatform, ExecutionPlatformError, nvidia_cdi_gpu_selector};
use dotcore_node_hardware::{
    MachineDescription,
    hardware::{AcceleratorUnavailable, AcceleratorVendorAdapter, UnavailableReason},
};
use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};
use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};

use crate::initial::request::{
    InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
};

/// CDI specification directory a described machine's Docker reports for its NVIDIA GPUs.
const DESCRIBED_CDI_SPEC_DIR: &str = "/etc/cdi";

/// NVIDIA facts and CDI selectors retained by an initialization capability report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialNvidiaProbeReport {
    facts: Vec<AcceleratorFacts>,
    cdi_selectors: BTreeSet<String>,
}

impl InitialNvidiaProbeReport {
    /// Returns per-device NVIDIA facts collected from the local NVML adapter.
    pub fn facts(&self) -> &[AcceleratorFacts] {
        &self.facts
    }

    /// Returns the discovered NVIDIA CDI selectors in stable lexical order.
    pub fn cdi_selectors(&self) -> impl Iterator<Item = &str> {
        self.cdi_selectors.iter().map(String::as_str)
    }
}

/// Apple GPU observed by an initialization capability report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitialAppleProbeReport {
    device_id: String,
    facts: AcceleratorFacts,
}

impl InitialAppleProbeReport {
    /// Returns the node-scoped device id the observation reported for the Apple GPU.
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    /// Returns the facts observed for the Apple GPU.
    pub fn facts(&self) -> &AcceleratorFacts {
        &self.facts
    }
}

/// Report of read-only capabilities selected by one initial site request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InitialSiteProbeReport {
    docker: Option<DockerCapabilityReport>,
    nvidia: Option<InitialNvidiaProbeReport>,
    apple: Option<InitialAppleProbeReport>,
}

impl InitialSiteProbeReport {
    /// Returns Docker facts when the request requires a Docker runtime capability.
    pub fn docker(&self) -> Option<&DockerCapabilityReport> {
        self.docker.as_ref()
    }

    /// Returns NVIDIA inventory and CDI selectors when NVIDIA inference was selected.
    pub fn nvidia(&self) -> Option<&InitialNvidiaProbeReport> {
        self.nvidia.as_ref()
    }

    /// Returns the observed Apple GPU when Apple inference was selected.
    pub fn apple(&self) -> Option<&InitialAppleProbeReport> {
        self.apple.as_ref()
    }

    /// Returns what probing a host of `machine` would report with Docker running Linux on the
    /// machine's architecture: its NVIDIA GPUs, each named by its full-GPU CDI selector, and its
    /// Apple GPU. Docker reports CDI only when the machine has NVIDIA GPUs for it to inject.
    pub fn described(machine: &MachineDescription) -> Result<Self, ExecutionPlatformError> {
        let (nvidia, others): (Vec<_>, Vec<_>) = machine
            .accelerator_facts()
            .into_iter()
            .partition(|facts| facts.vendor == AcceleratorVendor::Nvidia as i32);
        let nvidia = (!nvidia.is_empty()).then(|| InitialNvidiaProbeReport {
            cdi_selectors: nvidia
                .iter()
                .filter_map(|facts| facts.vendor_index.map(nvidia_cdi_gpu_selector))
                .collect(),
            facts: nvidia,
        });
        let apple = others
            .into_iter()
            .find(|facts| facts.vendor == AcceleratorVendor::Apple as i32)
            .map(|facts| InitialAppleProbeReport {
                device_id: facts.device_id.clone(),
                facts,
            });
        let docker_platform =
            ExecutionPlatform::normalize("linux", machine.platform().architecture())?;
        let cdi_spec_dirs = nvidia.as_ref().map(|_| DESCRIBED_CDI_SPEC_DIR);
        Ok(Self {
            docker: Some(DockerCapabilityReport::new(docker_platform, cdi_spec_dirs)),
            nvidia,
            apple,
        })
    }
}

/// Read-only initial site probe composed from Docker, NVML, and optional Apple GPU capability
/// adapters.
pub struct InitialSiteProber {
    docker: Box<dyn DockerCapabilityProbe>,
    nvidia: Box<dyn AcceleratorVendorAdapter>,
    apple: Option<Box<dyn AcceleratorVendorAdapter>>,
}

impl InitialSiteProber {
    /// Creates a prober that owns its explicit local capability adapters.
    pub fn new(
        docker: Box<dyn DockerCapabilityProbe>,
        nvidia: Box<dyn AcceleratorVendorAdapter>,
    ) -> Self {
        Self {
            docker,
            nvidia,
            apple: None,
        }
    }

    /// Adds the adapter that observes the Apple GPU; hosts without one cannot select Apple
    /// inference.
    pub fn with_apple_accelerator(mut self, apple: Box<dyn AcceleratorVendorAdapter>) -> Self {
        self.apple = Some(apple);
        self
    }

    /// Probes only the host capabilities selected by `request` without mutating site inputs.
    pub async fn probe(
        &self,
        request: &InitialSiteRequest,
    ) -> Result<InitialSiteProbeReport, InitialSiteProbeError> {
        let nvidia_requested = request
            .inference()
            .contains(&InitialInferenceRuntime::Nvidia);
        let docker_requested = nvidia_requested
            || request
                .applications()
                .contains(&InitialApplicationRuntime::Docker);
        let docker = if docker_requested {
            Some(
                self.docker
                    .probe()
                    .await
                    .map_err(InitialSiteProbeError::DockerUnavailable)?,
            )
        } else {
            None
        };
        let nvidia = if nvidia_requested {
            let facts = self
                .nvidia
                .collect_accelerator_facts()
                .map_err(InitialSiteProbeError::nvidia_unavailable)?;
            if facts.is_empty() {
                return Err(InitialSiteProbeError::nvidia_unavailable(
                    AcceleratorUnavailable::stable(UnavailableReason::NoDevice),
                ));
            }
            let docker = docker.as_ref().expect("NVIDIA requires Docker probing");
            if !docker.cdi_enabled() {
                return Err(InitialSiteProbeError::DockerCdiUnavailable);
            }
            let cdi_selectors = self
                .docker
                .nvidia_cdi_selectors()
                .await
                .map_err(InitialSiteProbeError::NvidiaCdiUnavailable)?;
            if cdi_selectors.is_empty() {
                return Err(InitialSiteProbeError::NvidiaCdiSelectorsUnavailable);
            }
            Some(InitialNvidiaProbeReport {
                facts,
                cdi_selectors,
            })
        } else {
            None
        };
        let apple = if request
            .inference()
            .contains(&InitialInferenceRuntime::Apple)
        {
            Some(self.probe_apple()?)
        } else {
            None
        };
        Ok(InitialSiteProbeReport {
            docker,
            nvidia,
            apple,
        })
    }

    fn probe_apple(&self) -> Result<InitialAppleProbeReport, InitialSiteProbeError> {
        let adapter = self.apple.as_ref().ok_or_else(|| {
            InitialSiteProbeError::apple_unavailable(AcceleratorUnavailable::stable(
                UnavailableReason::Unsupported,
            ))
        })?;
        let facts = adapter
            .collect_accelerator_facts()
            .map_err(InitialSiteProbeError::apple_unavailable)?;
        let facts = facts
            .into_iter()
            .find(|facts| facts.vendor == AcceleratorVendor::Apple as i32)
            .ok_or_else(|| {
                InitialSiteProbeError::apple_unavailable(AcceleratorUnavailable::stable(
                    UnavailableReason::NoDevice,
                ))
            })?;
        Ok(InitialAppleProbeReport {
            device_id: facts.device_id.clone(),
            facts,
        })
    }
}

/// Selected initial site capability requirements could not be established.
#[derive(Debug, thiserror::Error)]
pub enum InitialSiteProbeError {
    /// Docker was required but the daemon or its required Engine API could not be reached.
    #[error(
        "Docker runtime is unavailable; start a reachable Docker daemon with its Engine API enabled"
    )]
    DockerUnavailable(#[source] anyhow::Error),

    /// NVIDIA inference was required but NVML could not establish a usable inventory.
    #[error(
        "NVIDIA inference is unavailable: {reason:?}; install and authorize the NVIDIA driver, then verify a GPU is present"
    )]
    NvidiaUnavailable {
        reason: UnavailableReason,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    /// Apple inference was required but no Apple Silicon GPU could be observed.
    #[error(
        "Apple inference is unavailable: {reason:?}; run on an Apple Silicon Mac with a Metal GPU"
    )]
    AppleUnavailable {
        reason: UnavailableReason,
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
    },

    /// NVIDIA inference requires Docker CDI device injection.
    #[error(
        "NVIDIA inference requires Docker CDI device injection; enable Docker CDI and restart the daemon"
    )]
    DockerCdiUnavailable,

    /// NVIDIA CDI selector discovery failed.
    #[error(
        "NVIDIA CDI selector discovery failed; generate CDI specifications with nvidia-ctk cdi generate: {0}"
    )]
    NvidiaCdiUnavailable(#[source] anyhow::Error),

    /// NVIDIA CDI reported no selectors after both discovery paths completed.
    #[error(
        "NVIDIA CDI reports no selectors; generate a specification with nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml"
    )]
    NvidiaCdiSelectorsUnavailable,
}

impl InitialSiteProbeError {
    fn nvidia_unavailable(error: AcceleratorUnavailable) -> Self {
        let (reason, source) = error.into_parts();
        Self::NvidiaUnavailable { reason, source }
    }

    fn apple_unavailable(error: AcceleratorUnavailable) -> Self {
        let (reason, source) = error.into_parts();
        Self::AppleUnavailable { reason, source }
    }
}

#[cfg(test)]
pub fn test_probe_report(
    docker: bool,
    nvidia: Option<Vec<AcceleratorFacts>>,
    cdi_selectors: impl IntoIterator<Item = impl Into<String>>,
) -> InitialSiteProbeReport {
    test_probe_report_for_platform(
        docker.then(dotcore_domain::ExecutionPlatform::linux_amd64),
        nvidia,
        cdi_selectors,
    )
}

#[cfg(test)]
pub fn test_apple_probe_report(device_id: &str) -> InitialSiteProbeReport {
    InitialSiteProbeReport {
        apple: Some(InitialAppleProbeReport {
            device_id: device_id.to_owned(),
            facts: apple_facts(device_id),
        }),
        ..InitialSiteProbeReport::default()
    }
}

#[cfg(test)]
fn apple_facts(device_id: &str) -> AcceleratorFacts {
    AcceleratorFacts {
        vendor: AcceleratorVendor::Apple as i32,
        model: "Apple M3 Max".to_owned(),
        memory_architecture:
            dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Unified as i32,
        total_memory_bytes: None,
        driver_or_runtime_version: String::new(),
        device_id: device_id.to_owned(),
        vendor_index: Some(0),
        used_memory_bytes: None,
        working_set_limit_bytes: Some(28_991_029_248),
        supported_signals: Vec::new(),
    }
}

#[cfg(test)]
pub fn test_probe_report_for_platform(
    platform: Option<dotcore_domain::ExecutionPlatform>,
    nvidia: Option<Vec<AcceleratorFacts>>,
    cdi_selectors: impl IntoIterator<Item = impl Into<String>>,
) -> InitialSiteProbeReport {
    InitialSiteProbeReport {
        apple: None,
        docker: platform.map(|platform| DockerCapabilityReport::new(platform, ["/etc/cdi"])),
        nvidia: nvidia.map(|facts| InitialNvidiaProbeReport {
            facts,
            cdi_selectors: cdi_selectors.into_iter().map(Into::into).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        future::Future,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
    };

    use dotcore_node_hardware::hardware::{
        AcceleratorUnavailable, AcceleratorVendorAdapter, UnavailableReason,
    };
    use dotcore_proto::dotcore::observation::v1::{AcceleratorFacts, AcceleratorVendor};
    use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};

    use super::{InitialSiteProbeError, InitialSiteProber};
    use crate::initial::request::{
        InitialApplicationRuntime, InitialInferenceRuntime, InitialSiteRequest,
    };

    #[tokio::test]
    async fn nvidia_probe_combines_inventory_docker_cdi_and_selectors() {
        let docker =
            FakeDocker::available(["/etc/cdi"], ["nvidia.com/gpu=gpu0", "nvidia.com/gpu=all"]);
        let nvidia = FakeNvidia::available();
        let docker_calls = Arc::clone(&docker.probe_calls);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let report = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Simulated],
            ))
            .await
            .expect("NVIDIA requirements");

        assert_eq!(docker_calls.load(Ordering::SeqCst), 1);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 1);
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            report.nvidia().expect("NVIDIA report").facts(),
            nvidia_facts().as_slice()
        );
        assert_eq!(
            report
                .nvidia()
                .expect("NVIDIA report")
                .cdi_selectors()
                .collect::<Vec<_>>(),
            ["nvidia.com/gpu=all", "nvidia.com/gpu=gpu0"]
        );
        assert!(report.docker().expect("Docker report").cdi_enabled());
    }

    #[test]
    fn nvidia_unavailable_retains_detailed_loader_source() {
        #[derive(Debug, thiserror::Error)]
        #[error(
            "NVML library discovery failed: libnvidia-ml.so.1; /usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1: cannot open shared object file"
        )]
        struct LoaderDiagnostic;

        let error = InitialSiteProbeError::nvidia_unavailable(AcceleratorUnavailable::with_source(
            UnavailableReason::MissingLibrary,
            LoaderDiagnostic,
        ));
        let rendered = format!("{:#}", anyhow::Error::new(error));

        assert!(rendered.contains("NVIDIA inference is unavailable: MissingLibrary"));
        assert!(rendered.contains("libnvidia-ml.so.1"));
        assert!(rendered.contains("/usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1"));
        assert!(rendered.contains("cannot open shared object file"));
    }

    #[tokio::test]
    async fn docker_application_probe_does_not_require_nvidia_or_cdi_selectors() {
        let docker = FakeDocker::available([], []);
        let nvidia = FakeNvidia::unavailable(UnavailableReason::MissingLibrary);
        let docker_calls = Arc::clone(&docker.probe_calls);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let report = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Docker],
            ))
            .await
            .expect("Docker applications requirements");

        assert_eq!(docker_calls.load(Ordering::SeqCst), 1);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 0);
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            report.docker().expect("Docker report").cdi_spec_dirs(),
            &[] as &[String]
        );
        assert!(report.nvidia().is_none());
    }

    #[tokio::test]
    async fn apple_inference_reports_the_observed_device_without_docker() {
        let docker = FakeDocker::unavailable("Docker must not be probed");
        let docker_calls = Arc::clone(&docker.probe_calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(FakeNvidia::absent()))
            .with_apple_accelerator(Box::new(FakeApple(Ok(vec![super::apple_facts(
                "apple-gpu-0",
            )]))));

        let report = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Apple],
                [InitialApplicationRuntime::Simulated],
            ))
            .await
            .expect("Apple requirements");

        let apple = report.apple().expect("Apple report");
        assert_eq!(apple.device_id(), "apple-gpu-0");
        assert_eq!(apple.facts().model, "Apple M3 Max");
        assert_eq!(docker_calls.load(Ordering::SeqCst), 0);
        assert!(report.nvidia().is_none());
    }

    #[tokio::test]
    async fn apple_inference_without_an_observed_gpu_is_unavailable() {
        for (apple, reason) in [
            (None, UnavailableReason::Unsupported),
            (
                Some(FakeApple(Err(UnavailableReason::NoDevice))),
                UnavailableReason::NoDevice,
            ),
            (Some(FakeApple(Ok(Vec::new()))), UnavailableReason::NoDevice),
        ] {
            let prober = InitialSiteProber::new(
                Box::new(FakeDocker::available([], [])),
                Box::new(FakeNvidia::absent()),
            );
            let prober = match apple {
                Some(apple) => prober.with_apple_accelerator(Box::new(apple)),
                None => prober,
            };

            let error = prober
                .probe(&InitialSiteRequest::new(
                    [InitialInferenceRuntime::Apple],
                    [InitialApplicationRuntime::Simulated],
                ))
                .await
                .expect_err("Apple GPU must be observed");

            assert!(
                matches!(&error, InitialSiteProbeError::AppleUnavailable { reason: actual, .. } if *actual == reason),
                "{error:?}"
            );
            assert!(error.to_string().contains("Apple Silicon Mac"));
        }
    }

    #[tokio::test]
    async fn unavailable_docker_cdi_stops_before_selector_discovery() {
        let docker = FakeDocker::available([], ["nvidia.com/gpu=gpu0"]);
        let nvidia = FakeNvidia::available();
        let docker_calls = Arc::clone(&docker.probe_calls);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let error = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Docker],
            ))
            .await
            .expect_err("CDI must be enabled");

        assert!(matches!(error, InitialSiteProbeError::DockerCdiUnavailable));
        assert!(error.to_string().contains("enable Docker CDI"));
        assert_eq!(docker_calls.load(Ordering::SeqCst), 1);
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unavailable_selected_capabilities_are_actionable_without_follow_up_probes() {
        let docker = FakeDocker::unavailable("daemon socket refused connection");
        let nvidia = FakeNvidia::available();
        let docker_calls = Arc::clone(&docker.probe_calls);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let error = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Simulated],
                [InitialApplicationRuntime::Docker],
            ))
            .await
            .expect_err("Docker daemon must be reachable");

        assert!(matches!(error, InitialSiteProbeError::DockerUnavailable(_)));
        assert!(
            error
                .to_string()
                .contains("start a reachable Docker daemon")
        );
        assert_eq!(docker_calls.load(Ordering::SeqCst), 1);
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 0);

        let docker = FakeDocker::available(["/etc/cdi"], ["nvidia.com/gpu=gpu0"]);
        let nvidia = FakeNvidia::unavailable(UnavailableReason::NoDevice);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let error = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Simulated],
            ))
            .await
            .expect_err("NVIDIA device must be available");

        assert!(matches!(
            error,
            InitialSiteProbeError::NvidiaUnavailable {
                reason: UnavailableReason::NoDevice,
                ..
            }
        ));
        assert!(error.to_string().contains("verify a GPU is present"));
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn selected_nvidia_rejects_absent_devices_even_with_stale_cdi_selectors() {
        let docker = FakeDocker::available(["/etc/cdi"], ["nvidia.com/gpu=gpu0"]);
        let selector_calls = Arc::clone(&docker.selector_calls);
        let nvidia = FakeNvidia::absent();
        let nvidia_calls = std::sync::Arc::clone(&nvidia.calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(nvidia));

        let error = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Simulated],
            ))
            .await
            .expect_err("NVIDIA device must be present");

        assert!(matches!(
            error,
            InitialSiteProbeError::NvidiaUnavailable {
                reason: UnavailableReason::NoDevice,
                ..
            }
        ));
        assert_eq!(nvidia_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(selector_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn selector_discovery_errors_are_actionable_and_called_once() {
        let docker = FakeDocker::selector_error("cannot read /etc/cdi");
        let selector_calls = Arc::clone(&docker.selector_calls);
        let prober = InitialSiteProber::new(Box::new(docker), Box::new(FakeNvidia::available()));

        let error = prober
            .probe(&InitialSiteRequest::new(
                [InitialInferenceRuntime::Nvidia],
                [InitialApplicationRuntime::Simulated],
            ))
            .await
            .expect_err("selector discovery must succeed");

        assert!(matches!(
            error,
            InitialSiteProbeError::NvidiaCdiUnavailable(_)
        ));
        assert!(error.to_string().contains("nvidia-ctk cdi generate"));
        assert_eq!(selector_calls.load(Ordering::SeqCst), 1);
    }

    struct FakeDocker {
        probe_result: Result<DockerCapabilityReport, String>,
        selector_result: Result<BTreeSet<String>, String>,
        probe_calls: Arc<AtomicU8>,
        selector_calls: Arc<AtomicU8>,
    }

    impl FakeDocker {
        fn available(
            cdi_spec_dirs: impl IntoIterator<Item = &'static str>,
            selectors: impl IntoIterator<Item = &'static str>,
        ) -> Self {
            Self {
                probe_result: Ok(DockerCapabilityReport::new(
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                    cdi_spec_dirs,
                )),
                selector_result: Ok(selectors.into_iter().map(str::to_owned).collect()),
                probe_calls: Arc::new(AtomicU8::new(0)),
                selector_calls: Arc::new(AtomicU8::new(0)),
            }
        }

        fn unavailable(message: impl Into<String>) -> Self {
            Self {
                probe_result: Err(message.into()),
                selector_result: Ok(BTreeSet::new()),
                probe_calls: Arc::new(AtomicU8::new(0)),
                selector_calls: Arc::new(AtomicU8::new(0)),
            }
        }

        fn selector_error(message: impl Into<String>) -> Self {
            Self {
                probe_result: Ok(DockerCapabilityReport::new(
                    dotcore_domain::ExecutionPlatform::linux_amd64(),
                    ["/etc/cdi"],
                )),
                selector_result: Err(message.into()),
                probe_calls: Arc::new(AtomicU8::new(0)),
                selector_calls: Arc::new(AtomicU8::new(0)),
            }
        }
    }

    impl DockerCapabilityProbe for FakeDocker {
        fn probe(
            &self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>>
        {
            self.probe_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.probe_result.clone().map_err(anyhow::Error::msg);
            Box::pin(async move { result })
        }

        fn nvidia_cdi_selectors(
            &self,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
            self.selector_calls.fetch_add(1, Ordering::SeqCst);
            let result = self.selector_result.clone().map_err(anyhow::Error::msg);
            Box::pin(async move { result })
        }
    }

    struct FakeNvidia {
        result: Result<Vec<AcceleratorFacts>, UnavailableReason>,
        calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl FakeNvidia {
        fn available() -> Self {
            Self {
                result: Ok(nvidia_facts()),
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn absent() -> Self {
            Self {
                result: Ok(Vec::new()),
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn unavailable(reason: UnavailableReason) -> Self {
            Self {
                result: Err(reason),
                calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl AcceleratorVendorAdapter for FakeNvidia {
        fn vendor(&self) -> AcceleratorVendor {
            AcceleratorVendor::Nvidia
        }

        fn collect_accelerator_facts(
            &self,
        ) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.result.clone().map_err(AcceleratorUnavailable::stable)
        }
    }

    struct FakeApple(Result<Vec<AcceleratorFacts>, UnavailableReason>);

    impl AcceleratorVendorAdapter for FakeApple {
        fn vendor(&self) -> AcceleratorVendor {
            AcceleratorVendor::Apple
        }

        fn collect_accelerator_facts(
            &self,
        ) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
            self.0.clone().map_err(AcceleratorUnavailable::stable)
        }
    }

    fn nvidia_facts() -> Vec<AcceleratorFacts> {
        vec![AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA RTX PRO 6000 Blackwell".to_owned(),
            memory_architecture:
                dotcore_proto::dotcore::observation::v1::AcceleratorMemoryArchitecture::Dedicated
                    as i32,
            total_memory_bytes: Some(100_000_000_000),
            driver_or_runtime_version: "570.42.01".to_owned(),
            device_id: String::new(),
            vendor_index: Some(0),
            used_memory_bytes: None,
            working_set_limit_bytes: None,
            supported_signals: Vec::new(),
        }]
    }
}
