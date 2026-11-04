//! Runtime backends (crate `dotcore-runtime-backends`) that realize the owner-neutral
//! `dotcore-runtime` contracts on a node: the Docker backend over the local Docker Engine, the
//! native backend running launcher-held host processes, the deterministic simulated backend, and
//! the backend registry: canonical backend identifiers and the stopped-site cleanup composed from
//! the backends a site configuration binds.
//!
//! Backends import only the contracts, domain, protocol bindings and external crates, so they
//! never see an owner, controller or the site; composition hands them to the layers above as
//! contract trait objects.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

pub mod docker;
pub mod native;
pub mod registry;
pub mod simulated;
mod site_reset;
