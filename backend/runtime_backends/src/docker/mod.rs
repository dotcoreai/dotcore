//! Docker implementation of the infrastructure-neutral runtime boundary: this is the
//! module that actually creates, starts, stops, and inspects model workload containers
//! on the local Docker host.
//!
//! It implements both the individual-workload and the workload-group lifecycle
//! contracts. Every container and network Dotcore creates carries the `com.dotcore.*`
//! ownership labels, and resources without the complete label set are treated as
//! foreign and never touched.

mod adapter;
pub mod capabilities;
mod client;
mod configuration;
mod group;
mod logs;
mod ownership;
mod request;
mod site_data;
mod site_reset;
mod storage;
mod storage_cleanup;
mod validation;
mod workload_cache;

use anyhow::anyhow;

pub use adapter::DockerRuntimeBackend;
pub use adapter::{BACKEND_ID, DockerDevelopmentProcess};
#[cfg(any(test, feature = "test-support"))]
pub use configuration::DockerBackendFuture;
#[cfg(any(test, feature = "test-support"))]
pub use configuration::DockerRuntimeConfig;
pub use configuration::{
    DockerAcceleratorTransport, DockerBackendFactory, DockerRuntimeFactory,
    validated_accelerator_transport,
};
pub use group::{DockerGroupCreationError, DockerGroupStartError, OwnedWorkloadGroupRealization};
pub use ownership::OWNERSHIP_SCHEMA;
pub use request::ContainerTokens;
#[cfg(any(test, feature = "test-support"))]
pub use site_data::SITE_DATA_CLEANUP_IMAGE;
pub use site_data::cleanup_root_owned_site_data;
pub(crate) use site_reset::site_reset_adapter;
pub use storage_cleanup::{
    DockerMountedFolders, MountedFolders, MountedSourcesFuture, StorageArea, StorageCleanup,
    StorageRemoval, StorageRemovalOutcome,
};
pub use workload_cache::remove_site_workload_caches;

fn runtime_error(message: impl Into<String>) -> dotcore_runtime::RuntimeBackendError {
    dotcore_runtime::RuntimeBackendError::InvalidLaunch(anyhow!(message.into()))
}

fn runtime_error_with_source(
    message: impl Into<String>,
    source: impl std::error::Error + Send + Sync + 'static,
) -> dotcore_runtime::RuntimeBackendError {
    dotcore_runtime::RuntimeBackendError::docker_with_source(message, source)
}

#[cfg(test)]
mod tests {
    #[test]
    fn docker_sources_have_no_owner_or_protocol_policy() {
        for (name, source) in [
            ("adapter.rs", include_str!("adapter.rs")),
            ("client.rs", include_str!("client.rs")),
            ("group/mod.rs", include_str!("group/mod.rs")),
            ("request.rs", include_str!("request.rs")),
            ("ownership.rs", include_str!("ownership.rs")),
            ("validation.rs", include_str!("validation.rs")),
        ] {
            dotcore_runtime::test_support::assert_owner_neutral(name, source);
        }
    }
}
