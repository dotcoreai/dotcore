//! Route-manager-owned desired inference route declarations, immutable snapshots, derived availability, and management API adapter.

mod projection;
mod repository;
mod service;

pub(crate) use projection::{CompiledRouteSnapshot, RouteAvailabilityTargetsSource};
pub(crate) use repository::{
    PreparedExposureRoute, RouteDeclarationRepository, RouteRepositoryError,
};
pub(crate) use service::{RouteManager, RouteService};
