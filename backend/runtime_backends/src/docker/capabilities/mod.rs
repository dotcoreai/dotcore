//! Read-only local Docker daemon and NVIDIA CDI capability discovery.

mod daemon;
mod nvidia_cdi;

use std::{collections::BTreeSet, future::Future, pin::Pin};

use anyhow::Context;
use dotcore_runtime::{DockerCapabilityProbe, DockerCapabilityReport};

pub(super) use daemon::{execution_platform_from_information, probe_connected_daemon};

/// System probe of the local Docker daemon and its NVIDIA CDI devices.
pub struct SystemDockerCapabilityProbe;

impl DockerCapabilityProbe for SystemDockerCapabilityProbe {
    fn probe(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<DockerCapabilityReport>> + Send + '_>> {
        Box::pin(async {
            let daemon = daemon::probe_local_daemon()
                .await
                .context("local Docker Engine probe")?;
            Ok(DockerCapabilityReport::new(
                daemon.execution_platform,
                daemon.cdi_spec_dirs,
            ))
        })
    }

    fn nvidia_cdi_selectors(
        &self,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<BTreeSet<String>>> + Send + '_>> {
        Box::pin(nvidia_cdi::discover_selectors())
    }
}
