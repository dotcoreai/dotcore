//! Shared fakes and fixtures for the site command tests.
//!
//! Provides fixed identities, fake capability probes, a machine with one LAN address, and temporary
//! site roots.

use std::{
    collections::BTreeSet,
    fs,
    future::Future,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::LazyLock,
};

use dotcore_domain::SiteNodeHost;
use dotcore_node_hardware::hardware::{
    AcceleratorUnavailable, AcceleratorVendorAdapter, UnavailableReason,
};
use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};
use dotcore_site::initial::{
    probe::InitialSiteProber,
    request::{
        InitialSiteIdentityGenerator, InitialSiteIdentityKind, InitialSiteRequest,
        RandomInitialSiteIdentityGenerator,
    },
};

use super::{
    ApplicationsChoice, InferenceChoice, InitialSiteConfigurationArgs, SiteInitArgs, SiteLocation,
    SiteResetArgs,
    collection::{CollectionAdapters, request_from_arguments},
};
use crate::{
    hosts::{SelectedHost, test_support::FakeHostEnvironment},
    runtime_confirmation::RuntimeRemovalArgs,
};

/// A machine with one LAN address, no tailnet and no resolvable host name.
static LAN_MACHINE: LazyLock<FakeHostEnvironment> = LazyLock::new(|| {
    FakeHostEnvironment::default().with_interface("en0", Ipv4Addr::new(192, 0, 2, 10))
});

/// Random identities that suggest `name` as the node name.
pub(crate) struct NamedIdentities {
    random: RandomInitialSiteIdentityGenerator,
    name: Option<&'static str>,
}

impl InitialSiteIdentityGenerator for NamedIdentities {
    fn generate(&mut self, kind: InitialSiteIdentityKind) -> anyhow::Result<u64> {
        self.random.generate(kind)
    }

    fn node_display_name(&self) -> Option<String> {
        self.name.map(str::to_owned)
    }
}

pub(crate) fn named_identities(name: Option<&'static str>) -> NamedIdentities {
    NamedIdentities {
        random: RandomInitialSiteIdentityGenerator,
        name,
    }
}

/// Collection adapters over `prober` and `identities` on [`lan_machine`].
pub(super) fn adapters<'a>(
    prober: &'a InitialSiteProber,
    identities: &'a mut dyn InitialSiteIdentityGenerator,
) -> CollectionAdapters<'a> {
    CollectionAdapters {
        prober,
        environment: lan_machine(),
        identities,
    }
}

/// The shared machine with one LAN address, no tailnet and no resolvable host name.
pub(crate) fn lan_machine() -> &'static FakeHostEnvironment {
    &LAN_MACHINE
}

/// The request complete `arguments` make with their named host as the node host.
pub(crate) fn flagged_request(arguments: &InitialSiteConfigurationArgs) -> InitialSiteRequest {
    let host = arguments.host.clone().expect("arguments name a host");
    request_from_arguments(arguments, SelectedHost::explicit(host)).expect("request")
}

pub(crate) fn location(site_dir: &Path) -> SiteLocation {
    SiteLocation {
        site_dir: site_dir.to_owned(),
        catalog_dir: None,
    }
}

pub(crate) fn init_arguments(applications: ApplicationsChoice) -> SiteInitArgs {
    SiteInitArgs {
        configuration: InitialSiteConfigurationArgs {
            inference: vec![InferenceChoice::Simulated.into()],
            applications: vec![applications],
            site_id: Some("site-test".to_owned()),
            node_id: Some("node-test".to_owned()),
            display_name: None,
            host: Some(SiteNodeHost::parse("192.0.2.10").expect("host")),
            nvidia_device: Vec::new(),
            native_workload_lifetime: None,
            non_interactive: true,
            interactive: false,
        },
        replace: false,
        if_missing: false,
        force: false,
        removal: RuntimeRemovalArgs {
            yes: false,
            skip_runtime_cleanup: false,
        },
    }
}

pub(crate) fn reset_arguments(
    inference: InferenceChoice,
    applications: ApplicationsChoice,
) -> SiteResetArgs {
    SiteResetArgs {
        configuration: InitialSiteConfigurationArgs {
            inference: vec![inference.into()],
            applications: vec![applications],
            site_id: Some("site-test".to_owned()),
            node_id: Some("node-test".to_owned()),
            display_name: None,
            host: Some(SiteNodeHost::parse("192.0.2.10").expect("host")),
            nvidia_device: Vec::new(),
            native_workload_lifetime: None,
            non_interactive: true,
            interactive: false,
        },
        removal: RuntimeRemovalArgs {
            yes: false,
            skip_runtime_cleanup: false,
        },
    }
}

pub(crate) struct FakeDocker;

impl DockerCapabilityProbe for FakeDocker {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async {
            Ok(DockerCapabilityReport::new(
                dotcore_domain::ExecutionPlatform::linux_amd64(),
                ["/etc/cdi"],
            ))
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(async { Ok(BTreeSet::from(["nvidia.com/gpu=gpu0".to_owned()])) })
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "NVML library discovery failed: libnvidia-ml.so.1; /usr/lib/aarch64-linux-gnu/libnvidia-ml.so.1: cannot open shared object file"
)]
struct FakeNvmlDiscoveryError;

pub(crate) struct FakeApple;

impl AcceleratorVendorAdapter for FakeApple {
    fn vendor(&self) -> dotcore_proto::dotcore::observation::v1::AcceleratorVendor {
        dotcore_proto::dotcore::observation::v1::AcceleratorVendor::Apple
    }

    fn collect_accelerator_facts(
        &self,
    ) -> Result<
        Vec<dotcore_proto::dotcore::observation::v1::AcceleratorFacts>,
        AcceleratorUnavailable,
    > {
        Ok(vec![
            dotcore_proto::dotcore::observation::v1::AcceleratorFacts {
                vendor: dotcore_proto::dotcore::observation::v1::AcceleratorVendor::Apple as i32,
                model: "Apple M3 Max".to_owned(),
                device_id: "apple-gpu-0".to_owned(),
                ..Default::default()
            },
        ])
    }
}

pub(crate) struct UnavailableNvidia;

impl AcceleratorVendorAdapter for UnavailableNvidia {
    fn vendor(&self) -> dotcore_proto::dotcore::observation::v1::AcceleratorVendor {
        dotcore_proto::dotcore::observation::v1::AcceleratorVendor::Nvidia
    }

    fn collect_accelerator_facts(
        &self,
    ) -> Result<
        Vec<dotcore_proto::dotcore::observation::v1::AcceleratorFacts>,
        AcceleratorUnavailable,
    > {
        Err(AcceleratorUnavailable::with_source(
            UnavailableReason::MissingLibrary,
            FakeNvmlDiscoveryError,
        ))
    }
}

pub(crate) struct SiteRoot {
    pub(crate) root: PathBuf,
}

impl SiteRoot {
    pub(crate) fn new() -> Self {
        let root = crate::test_support::canonical_temp_dir().join(format!(
            "dotcore-cli-site-init-{}",
            crate::test_support::unique_suffix()
        ));
        fs::create_dir(&root).expect("fixture root");
        Self { root }
    }
}

impl Drop for SiteRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
