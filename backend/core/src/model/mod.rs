//! The inference controller boundary: it owns the desired and current state
//! of every model deployment and drives the running workloads to match the
//! desired state.
//!
//! It reads catalog data but owns no catalog, transport, or route manager
//! state, so model deployment product state is visible only through this
//! controller.

use dotcore_site::{repository::ApplicationRouteReference, validation::SiteDeclarationRules};

mod advisory;
pub(crate) mod compatibility;
pub mod controller;
pub mod domain;
pub mod engine;
pub(crate) mod engine_pin;
mod placement_preview;
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the site recommendation read and the no-site path call it next"
    )
)]
mod recommendation;
pub mod repository;
pub mod resolution;
pub mod resolved;
pub(crate) mod selection;
pub mod service;
pub mod sizing_scenario;
pub mod state;
mod upstream;

/// Identifies one retained application binding that prevents model deployment teardown: the site
/// reports the application deployment that uses an inference route the teardown would remove.
pub type ModelTeardownApplicationRouteBlocker = ApplicationRouteReference;

/// The inference controller's rules for model declarations in the desired site, which site loading
/// and validation apply without naming the controller.
pub fn site_declaration_rules() -> SiteDeclarationRules {
    SiteDeclarationRules {
        validate_local_model_declaration: engine_pin::validate_node_resolved_declaration,
    }
}
