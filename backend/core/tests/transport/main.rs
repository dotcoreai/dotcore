//! Transport integration suites: the development gRPC-Web server, the private inter-node transport, OpenAI compatibility and route management.
//!
//! Each suite is one module of this test binary, so the suites share one build and link of the
//! common fixtures in `support`.

#[path = "../support/mod.rs"]
mod support;

mod openai_compatibility;
mod private_transport;
mod route_management;
mod server;
