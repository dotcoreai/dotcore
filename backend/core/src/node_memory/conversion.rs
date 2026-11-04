//! Turns the capacity component's node pools into the API's node memory: pools, segments and the
//! deployments a pool can't size, each named by its lane's deployment identity.

use dotcore_proto::dotcore::capacity::v1::{
    MemoryOwner, MemoryPool, MemoryPoolKind, MemorySegment, MemorySegmentSource, NodeMemory,
    NodeMemoryPool, NodeMemoryState, memory_owner, memory_segment,
};
use dotcore_site::node::target_inventory::{CapabilityObservation, NodeMemoryObservation};

use crate::hardware::capacity::{
    NodeCapacity, PoolKind, PoolView, Segment, SegmentSource, Workload, WorkloadKey, WorkloadLane,
};

/// One pool as the API draws it, shared with placement previews.
pub(crate) fn memory_pool(view: PoolView) -> MemoryPool {
    let device = view.device.unwrap_or_default();
    MemoryPool {
        kind: match view.kind {
            PoolKind::Device => MemoryPoolKind::Device,
            PoolKind::Host => MemoryPoolKind::Host,
        } as i32,
        device_id: device.id,
        device_name: device.model,
        device_index: device.vendor_index,
        total_bytes: view.total_bytes,
        segments: view.segments.into_iter().map(memory_segment).collect(),
        over_total_bytes: view.over_total_bytes,
    }
}

/// One node's memory from its reading and its `workloads`.
pub(super) fn node_memory(
    observation: &NodeMemoryObservation,
    workloads: &[Workload],
) -> NodeMemory {
    let state = match observation.reading {
        CapabilityObservation::Available(_) => NodeMemoryState::Available,
        CapabilityObservation::Stale(_) => NodeMemoryState::Stale,
        CapabilityObservation::Unavailable => NodeMemoryState::Unavailable,
    };
    NodeMemory {
        site_node_id: observation.site_node_id.clone(),
        state: state as i32,
        observed_at: observation
            .observed_at
            .map(crate::model::service::timestamp_from),
        pools: NodeCapacity::for_node(&observation.reading, workloads)
            .pools
            .into_iter()
            .map(|pool| NodeMemoryPool {
                pool: Some(memory_pool(pool.pool)),
                r#unsized: pool
                    .unsized_workloads
                    .into_iter()
                    .map(|key| MemoryOwner {
                        owner: Some(match key.lane {
                            WorkloadLane::Inference => {
                                memory_owner::Owner::ModelDeploymentId(key.id)
                            }
                            WorkloadLane::Applications => {
                                memory_owner::Owner::ApplicationDeploymentId(key.id)
                            }
                        }),
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn memory_segment(segment: Segment) -> MemorySegment {
    MemorySegment {
        source: match segment.source {
            SegmentSource::Measured => MemorySegmentSource::Measured,
            SegmentSource::Estimated => MemorySegmentSource::Estimated,
            SegmentSource::Claimed => MemorySegmentSource::Claimed,
            SegmentSource::Other => MemorySegmentSource::Other,
            SegmentSource::Previewed => MemorySegmentSource::Previewed,
        } as i32,
        owner: segment.workload.map(segment_owner),
        bytes: segment.bytes,
    }
}

fn segment_owner(key: WorkloadKey) -> memory_segment::Owner {
    match key.lane {
        WorkloadLane::Inference => memory_segment::Owner::ModelDeploymentId(key.id),
        WorkloadLane::Applications => memory_segment::Owner::ApplicationDeploymentId(key.id),
    }
}
