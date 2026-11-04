//! Fakes shared by the guided site initialization tests.
//!
//! Provides capability probes with fixed outcomes, fixed identities, and a temporary directory
//! removed after each test. Nothing touches the real host, so guided tests are deterministic.

use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use dotcore_node_hardware::hardware::{
    AcceleratorUnavailable, AcceleratorVendorAdapter, UnavailableReason,
};
use dotcore_proto::dotcore::observation::v1::{
    AcceleratorFacts, AcceleratorMemoryArchitecture, AcceleratorVendor,
};
use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};
use dotcore_site::initial::{
    probe::InitialSiteProber,
    request::{InitialSiteIdentityGenerator, InitialSiteIdentityKind},
};

use super::{GuidedInitialSiteCollection, GuidedInitialSiteDefaults, collect_guided_initial_site};
use crate::{
    hosts::{HostCandidate, SelectedHost, test_support::FakeHostEnvironment},
    terminal::ScriptedTerminal,
};

static TEMPORARY_ROOT_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Runs guided collection with only the site directory resolved and the recommended host among
/// `candidates` offered.
pub(super) fn collect(
    site_dir: PathBuf,
    terminal: &mut ScriptedTerminal,
    candidates: Vec<HostCandidate>,
    prober: &InitialSiteProber,
    identities: &mut FixedIdentities,
) -> anyhow::Result<GuidedInitialSiteCollection> {
    collect_with_defaults(
        GuidedInitialSiteDefaults::new(site_dir)
            .with_host(SelectedHost::recommended(&candidates))
            .with_host_candidates(candidates),
        terminal,
        prober,
        identities,
    )
}

/// Runs guided collection to completion on a fresh runtime, on a machine whose interfaces
/// cannot be read and that resolves no host name, so the host check stays silent except for
/// loopback.
pub(super) fn collect_with_defaults(
    defaults: GuidedInitialSiteDefaults,
    terminal: &mut ScriptedTerminal,
    prober: &InitialSiteProber,
    identities: &mut FixedIdentities,
) -> anyhow::Result<GuidedInitialSiteCollection> {
    crate::build_runtime()
        .expect("runtime")
        .block_on(collect_guided_initial_site(
            defaults,
            terminal,
            &FakeHostEnvironment::default(),
            prober,
            identities,
        ))
}

/// A prober that finds both Docker and an NVIDIA GPU.
pub(super) fn available_prober() -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(FakeDocker::available()),
        Box::new(FakeNvidia::available()),
    )
}

/// A prober that finds Docker and two NVIDIA GPUs, each with its own CDI selector.
pub(super) fn two_gpu_prober() -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(FakeDocker(BTreeSet::from([
            "nvidia.com/gpu=gpu0".to_owned(),
            "nvidia.com/gpu=gpu1".to_owned(),
        ]))),
        Box::new(FakeNvidia::with_count(2)),
    )
}

/// A prober that finds Docker but no NVIDIA GPU.
pub(super) fn unavailable_nvidia_prober() -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(FakeDocker::available()),
        Box::new(FakeNvidia::unavailable(UnavailableReason::NoDevice)),
    )
}

/// A prober that finds Docker and an NVIDIA driver that fails with `reason`.
pub(super) fn unusable_nvidia_prober(reason: UnavailableReason) -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(FakeDocker::available()),
        Box::new(FakeNvidia::unavailable(reason)),
    )
}

/// A prober whose NVIDIA failure carries a detailed library discovery source.
pub(super) fn detailed_unavailable_nvidia_prober() -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(FakeDocker::available()),
        Box::new(DetailedUnavailableNvidia),
    )
}

/// A prober on an Apple Silicon host: Docker is reachable, NVIDIA has no device, and the Apple
/// GPU is observed.
pub(super) fn apple_prober() -> InitialSiteProber {
    unavailable_nvidia_prober().with_apple_accelerator(Box::new(FakeApple))
}

/// A prober that cannot reach Docker, which also makes NVIDIA inference unavailable.
pub(super) fn unavailable_docker_prober() -> InitialSiteProber {
    InitialSiteProber::new(Box::new(FailingDocker), Box::new(FakeNvidia::available()))
}

/// A prober that counts how often each capability is probed.
pub(super) fn counting_prober(
    docker_calls: &Arc<AtomicUsize>,
    nvidia_calls: &Arc<AtomicUsize>,
) -> InitialSiteProber {
    InitialSiteProber::new(
        Box::new(CountingDocker(Arc::clone(docker_calls))),
        Box::new(CountingNvidia(Arc::clone(nvidia_calls))),
    )
}

/// Identities handed out in a fixed order, with an optional suggested SiteNode name.
pub(super) struct FixedIdentities {
    values: VecDeque<u64>,
    node_name: Option<String>,
}

impl FixedIdentities {
    pub(super) fn new(values: impl IntoIterator<Item = u64>) -> Self {
        Self {
            values: values.into_iter().collect(),
            node_name: None,
        }
    }

    pub(super) fn with_node_name(mut self, name: impl Into<String>) -> Self {
        self.node_name = Some(name.into());
        self
    }
}

impl InitialSiteIdentityGenerator for FixedIdentities {
    fn generate(&mut self, _kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
        self.values
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("missing fixed identity"))
    }

    fn node_display_name(&self) -> Option<String> {
        self.node_name.clone()
    }
}

/// A uniquely named directory removed when dropped.
pub(super) struct TemporaryRoot(PathBuf);

impl TemporaryRoot {
    pub(super) fn new() -> Self {
        let path = crate::test_support::canonical_temp_dir().join(format!(
            "dotcore-guided-site-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time")
                .as_nanos(),
            TEMPORARY_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).expect("temporary root");
        Self(path)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A reachable Docker daemon with NVIDIA CDI selectors.
pub(super) struct FakeDocker(BTreeSet<String>);

impl FakeDocker {
    fn available() -> Self {
        Self(BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()]))
    }
}

impl DockerCapabilityProbe for FakeDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async { Ok(docker_report()) })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        let selectors = self.0.clone();
        Box::pin(async move { Ok(selectors) })
    }
}

/// A Docker daemon whose probe fails with an I/O source.
pub(super) struct FailingDocker;

impl DockerCapabilityProbe for FailingDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async {
            Err(
                anyhow::Error::new(io::Error::other("injected Docker I/O failure"))
                    .context("injected Docker probe failure"),
            )
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(async { Ok(BTreeSet::new()) })
    }
}

struct CountingDocker(Arc<AtomicUsize>);

impl DockerCapabilityProbe for CountingDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(docker_report())
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(async { Ok(BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()])) })
    }
}

/// An NVIDIA adapter with a fixed inventory or unavailability reason.
pub(super) struct FakeNvidia(Result<Vec<AcceleratorFacts>, UnavailableReason>);

impl FakeNvidia {
    pub(super) fn available() -> Self {
        Self::with_count(1)
    }

    /// Observes `count` identical GPUs enumerated from index 0.
    pub(super) fn with_count(count: u32) -> Self {
        Self(Ok((0..count)
            .map(|index| AcceleratorFacts {
                vendor: AcceleratorVendor::Nvidia as i32,
                model: "NVIDIA RTX PRO 6000 Blackwell".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                total_memory_bytes: Some(100_000_000_000),
                driver_or_runtime_version: "570.42.01".to_owned(),
                device_id: String::new(),
                vendor_index: Some(index),
                used_memory_bytes: None,
                working_set_limit_bytes: None,
                supported_signals: Vec::new(),
            })
            .collect()))
    }

    pub(super) fn unavailable(reason: UnavailableReason) -> Self {
        Self(Err(reason))
    }
}

impl AcceleratorVendorAdapter for FakeNvidia {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        self.0.clone().map_err(AcceleratorUnavailable::stable)
    }
}

/// An Apple adapter that observes the one Apple GPU.
pub(super) struct FakeApple;

impl AcceleratorVendorAdapter for FakeApple {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Apple
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        Ok(vec![AcceleratorFacts {
            vendor: AcceleratorVendor::Apple as i32,
            model: "Apple M3 Max".to_owned(),
            memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
            device_id: "apple-gpu-0".to_owned(),
            ..Default::default()
        }])
    }
}

struct DetailedUnavailableNvidia;

impl AcceleratorVendorAdapter for DetailedUnavailableNvidia {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        Err(AcceleratorUnavailable::with_source(
            UnavailableReason::MissingLibrary,
            FakeNvmlDiscoveryError,
        ))
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "NVML library discovery failed: libnvidia-ml.so.1; /usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1: cannot open shared object file"
)]
struct FakeNvmlDiscoveryError;

struct CountingNvidia(Arc<AtomicUsize>);

impl AcceleratorVendorAdapter for CountingNvidia {
    fn vendor(&self) -> AcceleratorVendor {
        AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(&self) -> Result<Vec<AcceleratorFacts>, AcceleratorUnavailable> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(FakeNvidia::available().0.expect("available NVIDIA facts"))
    }
}

fn docker_report() -> DockerCapabilityReport {
    DockerCapabilityReport::new(
        dotcore_domain::ExecutionPlatform::linux_amd64(),
        ["/etc/cdi"],
    )
}
