//! Builds the placement preview a resource estimate returns for its target: what the target's
//! memory holds with the estimated configuration added, as the console draws it.
//!
//! The capacity component judges the configuration's demand against the target's pools, with what
//! the site's deployments hold there; this module only turns that answer into the API's terms. The
//! preview is advisory and transient: it reflects the latest observation it was built from, which
//! it names, and nothing of it is retained.

use std::time::SystemTime;

use dotcore_catalog::{ACCELERATOR_MEMORY_RESOURCE, HOST_MEMORY_RESOURCE};
use dotcore_proto::dotcore::capacity::v1::{MemoryPoolPreview, PlacementPreview};

use crate::{
    hardware::{
        capacity::{Estimate, PoolPreview, demand, preview},
        selection::SelectedTargetCapabilities,
    },
    model::resolution::ResolvedLocalModelValues,
};

/// Previews `resolved` on the target capabilities `selected`, whose pools already count what the
/// site's deployments hold there. Without `include_demand`, as when custom arguments may change the
/// footprint, the preview shows the target's occupancy alone and its fit reads unknown.
/// `observed_at` is when the target's memory was read.
pub(crate) fn placement_preview(
    resolved: &ResolvedLocalModelValues,
    selected: &SelectedTargetCapabilities,
    include_demand: bool,
    observed_at: Option<SystemTime>,
) -> PlacementPreview {
    let estimate = |resource: &str| {
        resolved
            .estimates
            .iter()
            .find(|estimate| estimate.resource == resource)
            .map(|estimate| estimate.value)
    };
    let demand = include_demand
        .then(|| {
            demand(
                &Estimate {
                    accelerator_bytes: estimate(ACCELERATOR_MEMORY_RESOURCE),
                    host_bytes: estimate(HOST_MEMORY_RESOURCE),
                },
                &selected.capacity,
            )
        })
        .flatten();
    let preview = preview(
        demand.as_ref(),
        &selected.capacity,
        resolved
            .selection
            .hardware_support
            .hardware_requirements
            .as_ref(),
    );
    PlacementPreview {
        pools: preview.pools.into_iter().map(pool_preview).collect(),
        free_memory_fit: preview.free_memory_fit as i32,
        observed_at: observed_at.map(crate::model::service::timestamp_from),
    }
}

fn pool_preview(pool: PoolPreview) -> MemoryPoolPreview {
    MemoryPoolPreview {
        pool: Some(crate::node_memory::memory_pool(pool.pool)),
        fit_line_bytes: pool.fit_line_bytes,
        free_memory_fit: pool.free_memory_fit as i32,
        over_by_bytes: pool.over_by_bytes,
        beyond_total: pool.beyond_total,
    }
}
