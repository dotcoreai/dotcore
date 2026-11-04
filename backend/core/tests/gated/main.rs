//! Gated suites that need a real Docker daemon, NVIDIA GPU or Apple Silicon host; each is ignored and selected by its hardware profile (`just backend::test-docker|test-nvidia|test-apple`).
//!
//! Each suite is one module of this test binary, so the suites share one build and link of the
//! common fixtures in `support`.

#[path = "../support/mod.rs"]
mod support;

mod apple_hardware_integration;
mod decision_requests;
mod docker_application_integration;
mod ninfer_nvidia_docker_integration;
mod node_hardware_sampling;
mod nvidia_docker_integration;
mod nvidia_qualification;
mod vllm_nvidia_docker_integration;
