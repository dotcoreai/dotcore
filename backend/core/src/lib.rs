//! The backend library crate: it owns model and application deployment
//! lifecycle, their current state, controller dispatch to the runtime backends,
//! and the gRPC transport, with each capability in its own module, and composes
//! them over the site, artifact, catalog, runtime contracts and runtime backends
//! crates.
//!
//! Every capability keeps product-state ownership in the component that owns
//! it, so a caller drives a feature through that component's boundary rather
//! than reaching into another module's records. Protocol bindings are
//! generated from the repository's proto schemas and never hand-edited.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

pub mod application;
pub mod composition;
#[doc(hidden)]
pub mod development;
pub(crate) mod dispatch;
pub mod logging;
pub mod model;
pub mod server;
pub mod telemetry;

#[cfg(feature = "test-support")]
pub mod testing;

pub(crate) mod collection_metrics;
pub(crate) mod deployment_logs;
pub(crate) mod diagnostic;
pub(crate) mod hardware;
pub(crate) mod management_logs;
pub(crate) mod metric_stream;
pub(crate) mod metrics;
pub(crate) mod node_memory;
pub(crate) mod resource_watch;
pub(crate) mod route_manager;
pub(crate) mod router;
pub(crate) mod storage;

pub(crate) mod inference;

#[cfg(test)]
pub(crate) mod test_support;
