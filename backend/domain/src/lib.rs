//! Shared domain and platform primitives that every backend layer needs: the stable identifier
//! grammar of catalog and site records, OCI and Nix references, SiteNode identities, hosts and site
//! directory paths, host process identity, deployment values, inference route rules, execution
//! platforms, accelerator facts and node hardware observations, the backend clock, durable file
//! flushes, and background loop failure logging.
//!
//! This is the lowest backend layer: it knows no owner, controller, site, catalog, or runtime
//! module, so any of them can depend on it without creating a cycle. Only protocol bindings, raw
//! host process facts, and external crates sit below it.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

mod accelerator;
pub mod background_loop;
pub mod clock;
pub mod deployment;
pub mod diagnostic;
pub mod docker;
pub mod fs_sync;
pub mod hardware_observation;
pub mod host_process;
mod id;
pub mod inference_route;
pub mod nix;
mod node_host;
pub mod oci;
mod paths;
mod platform;
mod site_node_id;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use accelerator::{
    APPLE_GPU_DEVICE_ID, Accelerator, AcceleratorDeviceSelector, AcceleratorResolution,
    AcceleratorSelectionError, MemoryArchitecture, NvidiaDriverMajorVersion, NvidiaDriverVersion,
    NvidiaDriverVersionError, ResolvedAccelerators, SelectableAcceleratorDevice,
    names_every_cdi_device, nvidia_cdi_gpu_index, nvidia_cdi_gpu_selector,
    nvidia_cdi_legacy_gpu_selector, selected_accelerator_facts,
};
pub use id::valid_id;
pub use node_host::{
    BindAddress, BindAddressError, HostResolutionError, HostResolver, SiteNodeHost,
    SiteNodeHostError, SystemHostResolver,
};
pub use paths::{
    CACHE_DIR, CATALOG_DIR, CacheLayout, HOME, ResolvedSitePaths, SITE_DIR, SitePaths,
    SitePathsError, SitePathsResult, XDG_CACHE_HOME, XDG_STATE_HOME,
};
pub use platform::{ExecutionPlatform, ExecutionPlatformError, WorkloadArtifactKind};
pub use site_node_id::{SiteNodeId, SiteNodeIdError};
