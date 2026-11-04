//! Multi-process site suites: separate manager and worker backend processes joining, observing, rejoining, being removed, and realizing model and application deployments.
//!
//! Each suite is one module of this test binary, so the suites share one build and link of the
//! common fixtures in `support`.

#[path = "../support/mod.rs"]
mod support;

mod metric_history;
mod metric_stream_faults;
mod node_rejoin;
mod node_removal;
mod remote_application_lifecycle;
mod remote_model_lifecycle;
mod remote_native_model_lifecycle;
mod site_diagnostics;
mod two_process_observation;
