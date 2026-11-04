//! Whole-backend integration suites: catalog contracts, lifecycle, persistence, upstream deployments, initial site startup, configuration, route exposure, declared machines and public module paths.
//!
//! Each suite is one module of this test binary, so the suites share one build and link of the
//! common fixtures in `support`.

#[path = "../support/mod.rs"]
mod support;

mod backend_integration;
mod catalog_contracts;
mod configuration_environment;
mod declared_machine;
mod initial_site_startup;
mod public_paths;
mod route_exposure_integrity;
mod upstream_integration;
