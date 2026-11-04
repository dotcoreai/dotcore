//! Native implementation of the infrastructure-neutral runtime boundary: it runs workloads as
//! host processes, each held by a `dotcore-launcher` process that can outlive the server.
//!
//! Every workload lives in its own directory under the site directory, and its launch record
//! carries ownership evidence naming the site, SiteNode, execution target, and the exact state
//! root that created it. Workloads whose evidence names another state root, such as a copied
//! site directory, are treated as foreign and never touched; untrustworthy evidence blocks
//! mutation.

mod adapter;
mod configuration;
mod launch;
mod launcher;
mod logs;
mod memory;
mod ownership;
mod ports;
mod probe;
mod record;
mod site_cleanup;
mod state_root;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use adapter::BACKEND_ID;
pub use adapter::{NativeRuntimeBackend, NativeWorkloadProcess};
pub use configuration::{DEFAULT_STORE_ROOT, NativeRuntimeConfig, NativeWorkloadLifetime};
pub use launcher::NativeLauncherError;
pub use launcher::resolve_launcher;
pub use probe::NativeProbe;
pub use site_cleanup::{NATIVE_RUNTIME_NAME, NativeSiteCleanup, NativeSiteCleanupError};
pub use state_root::NativeStateRoot;

#[cfg(test)]
mod tests {
    #[test]
    fn native_sources_have_no_owner_or_protocol_policy() {
        for (name, source) in [
            ("mod.rs", include_str!("mod.rs")),
            ("adapter.rs", include_str!("adapter.rs")),
            ("configuration.rs", include_str!("configuration.rs")),
            ("launch.rs", include_str!("launch.rs")),
            ("launcher.rs", include_str!("launcher.rs")),
            ("probe.rs", include_str!("probe.rs")),
            ("logs.rs", include_str!("logs.rs")),
            ("memory.rs", include_str!("memory.rs")),
            ("ownership.rs", include_str!("ownership.rs")),
            ("ports.rs", include_str!("ports.rs")),
            ("record.rs", include_str!("record.rs")),
            ("site_cleanup.rs", include_str!("site_cleanup.rs")),
            ("state_root.rs", include_str!("state_root.rs")),
            (
                "runtime/store/v1/native.proto",
                include_str!("../../../../proto/dotcore/runtime/store/v1/native.proto"),
            ),
        ] {
            dotcore_runtime::test_support::assert_owner_neutral(name, source);
        }
    }
}
