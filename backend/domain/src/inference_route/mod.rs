//! Shared rules over inference route declarations: the callable route-name grammar, the fallback
//! group shape a route must have, and what tearing down model deployments does to the routes that
//! reference them.
//!
//! The rules are pure value checks over route declarations, so the route manager applying a
//! route mutation and the site validating or accepting declarations reach the same answer.

mod name;
mod policy;

pub use name::is_valid_route_name;
pub use policy::{
    RouteShapeError, RouteUnloadPlan, classify_unload, classify_unload_set, validate_route_shape,
};
