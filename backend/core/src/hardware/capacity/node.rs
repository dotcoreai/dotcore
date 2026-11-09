//! A node's memory pools as views draw them: every pool of the node, whether or not an execution
//! target draws from it, with what its workloads hold there.
//!
//! The pools and what workloads hold in them follow the same rules as a target's pools, so a
//! node's pools that a target draws from read the same as that target's. A view also learns which
//! workloads it can't size in a pool, and how far drawn use goes past each total. Like every
//! capacity answer it is advice built from the latest reading and retains nothing.

use dotcore_domain::MemoryArchitecture;
use dotcore_site::node::target_inventory::{
    CapabilityObservation, DeviceMemoryFacts, NodeMemoryFacts, NodeMemoryReading,
};

use super::{NodeHardware, Pool, PoolView, Workload, WorkloadKey, occupy};

/// Every memory pool of one node with what occupies it: one pool per device with dedicated memory
/// in reported order, then the host pool, which unified devices share.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeCapacity {
    pub(crate) pools: Vec<NodePool>,
}

impl NodeCapacity {
    /// Builds a node's pools from its memory reading and counts in them what `workloads`, the
    /// node's workloads, hold there. A device of unknown memory architecture gets a pool of unknown
    /// size, since its memory may be the host's. A stale reading keeps its totals but no measured
    /// use, and an unavailable one gives no pools.
    pub(crate) fn for_node(
        memory: &CapabilityObservation<NodeMemoryReading>,
        workloads: &[Workload],
    ) -> Self {
        let (reading, fresh) = match memory {
            CapabilityObservation::Available(reading) => (reading, true),
            CapabilityObservation::Stale(reading) => (reading, false),
            CapabilityObservation::Unavailable => return Self::default(),
        };
        let devices = reading
            .devices()
            .filter_map(|device| match device.memory_architecture {
                Some(MemoryArchitecture::Dedicated) => Some(device.memory.clone()),
                Some(MemoryArchitecture::Unified) => None,
                None => Some(DeviceMemoryFacts {
                    total_bytes: None,
                    used_bytes: None,
                    ..device.memory.clone()
                }),
            })
            .collect();
        let hardware = NodeHardware::read(
            &NodeMemoryFacts {
                devices,
                host: reading.host(),
            },
            fresh,
        );
        let mut pools = hardware
            .devices
            .iter()
            .map(|device| device.pool())
            .chain([hardware.host_pool()])
            .collect::<Vec<_>>();
        occupy(&mut pools, workloads);
        Self {
            pools: pools
                .iter()
                .map(|pool| NodePool {
                    pool: pool.view(None),
                    unsized_workloads: unsized_in(pool, workloads),
                })
                .collect(),
        }
    }
}

/// One pool of a node as a view draws it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct NodePool {
    pub(crate) pool: PoolView,

    /// Workloads expected to hold memory in the pool that hold none there, so their size can't be
    /// drawn; in workload order.
    pub(crate) unsized_workloads: Vec<WorkloadKey>,
}

/// The workloads that draw from `pool` and are expected to hold memory there, but hold none.
fn unsized_in(pool: &Pool, workloads: &[Workload]) -> Vec<WorkloadKey> {
    workloads
        .iter()
        .filter(|workload| workload.expects_use && workload.draws_from(pool))
        .filter(|workload| {
            !pool
                .occupancy
                .workloads
                .iter()
                .any(|segment| segment.workload.as_ref() == Some(&workload.key))
        })
        .map(|workload| workload.key.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use dotcore_domain::timing::Timing;
    use std::{collections::BTreeMap, time::SystemTime};

    use dotcore_domain::MemoryArchitecture::{self, Dedicated};
    use dotcore_node_hardware::{MachineDescription, MachineWorkload};
    use dotcore_site::node::target_inventory::{
        CapabilityObservation, NodeAcceleratorObservation, NodeMemoryObservation,
        TargetCapabilityReconciler,
    };

    use super::{NodeCapacity, NodePool};
    use crate::hardware::capacity::{
        Estimate, NodeHardware, PoolDevice, PoolKind, PoolView, Segment, SegmentSource,
        TargetCapacity, Workload, WorkloadKey, WorkloadPhase, preview,
    };

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const RTX_5090_TOTAL: u64 = 32_607 * MIB;
    const RESERVED: u64 = 497 * MIB;
    const GB10_TOTAL: u64 = 130_596_048_896;
    const GB10_IDLE_USED: u64 = 7_938_146_304;

    #[test]
    fn a_dedicated_node_holds_a_pool_per_gpu_and_counts_claims_measured_use_and_applications() {
        let gpu = gpu_of("rtx-5090", 0);
        let estimate = Estimate {
            accelerator_bytes: Some(10 * GIB),
            host_bytes: None,
        };
        let model = |phase, attributed| Workload {
            phase,
            ..workload(WorkloadKey::inference("model"), &gpu, estimate, attributed)
        };
        let application = Workload {
            phase: WorkloadPhase::Other,
            ..workload(
                WorkloadKey::applications("app"),
                &gpu,
                Estimate::default(),
                Some(3 * GIB),
            )
        };
        let device = |segments| PoolView {
            kind: PoolKind::Device,
            device: Some(named_gpu("5090")),
            total_bytes: Some(RTX_5090_TOTAL),
            segments,
            over_total_bytes: 0,
        };
        let host = host_pool(64 * GIB, vec![other(4 * GIB)]);

        // Idle, the GPU holds its driver's reserved memory as other use.
        assert_eq!(
            node_pools("rtx-5090", &[], false, &[]),
            vec![device(vec![other(RESERVED)]), host.clone()]
        );
        // Starting, the model claims what its attributed use hasn't reached of its estimate.
        assert_eq!(
            node_pools(
                "rtx-5090",
                &[on_gpu(&gpu, 4 * GIB)],
                false,
                &[model(WorkloadPhase::Starting, Some(4 * GIB))],
            ),
            vec![
                device(vec![
                    measured("model", 4 * GIB),
                    claimed("model", 6 * GIB),
                    other(RESERVED),
                ]),
                host.clone(),
            ]
        );
        // Ready, it holds its measured use and other use is still the reserved memory.
        assert_eq!(
            node_pools(
                "rtx-5090",
                &[on_gpu(&gpu, 10 * GIB)],
                false,
                &[model(WorkloadPhase::Ready, Some(10 * GIB))],
            ),
            vec![
                device(vec![measured("model", 10 * GIB), other(RESERVED)]),
                host.clone(),
            ]
        );
        // An application's attributed memory leaves other use and becomes its own segment.
        let running = [on_gpu(&gpu, 10 * GIB), on_gpu(&gpu, 3 * GIB)];
        assert_eq!(
            node_pools(
                "rtx-5090",
                &running,
                false,
                &[model(WorkloadPhase::Ready, Some(10 * GIB))],
            ),
            vec![
                device(vec![measured("model", 10 * GIB), other(3 * GIB + RESERVED)]),
                host.clone(),
            ]
        );
        assert_eq!(
            node_pools(
                "rtx-5090",
                &running,
                false,
                &[model(WorkloadPhase::Ready, Some(10 * GIB)), application],
            ),
            vec![
                device(vec![
                    measured("model", 10 * GIB),
                    segment(
                        SegmentSource::Measured,
                        WorkloadKey::applications("app"),
                        3 * GIB
                    ),
                    other(RESERVED),
                ]),
                host,
            ]
        );
    }

    #[test]
    fn a_starting_model_whose_whole_estimate_is_attributed_counts_once_as_measured_use() {
        let gpu = gpu_of("rtx-5090", 0);
        let bytes = 5_530 * MIB;
        let starting = |attributed| Workload {
            phase: WorkloadPhase::Starting,
            ..workload(
                WorkloadKey::inference("model"),
                &gpu,
                Estimate {
                    accelerator_bytes: Some(bytes),
                    host_bytes: None,
                },
                attributed,
            )
        };
        let device = |segments, over_total_bytes| PoolView {
            kind: PoolKind::Device,
            device: Some(named_gpu("5090")),
            total_bytes: Some(RTX_5090_TOTAL),
            segments,
            over_total_bytes,
        };
        let host = host_pool(64 * GIB, vec![other(4 * GIB)]);

        // A sample and the attribution taken from it: measured use only, nothing claimed.
        assert_eq!(
            node_pools(
                "rtx-5090",
                &[on_gpu(&gpu, bytes)],
                false,
                &[starting(Some(bytes))],
            ),
            vec![
                device(vec![measured("model", bytes), other(RESERVED)], 0),
                host.clone(),
            ]
        );
        // A sample joined with attribution from before it counts the same bytes as other use and
        // as the whole claim, which is why the node sends them together.
        assert_eq!(
            node_pools("rtx-5090", &[on_gpu(&gpu, bytes)], false, &[starting(None)],),
            vec![
                device(vec![claimed("model", bytes), other(bytes + RESERVED)], 0),
                host,
            ]
        );
    }

    #[test]
    fn each_gpu_of_a_mixed_node_is_its_own_pool_and_unified_nodes_have_the_host_pool_only() {
        let gpu = gpu_of("rtx-4090+rtx-4080", 1);
        let ready = Workload {
            phase: WorkloadPhase::Ready,
            ..workload(
                WorkloadKey::inference("model"),
                &gpu,
                Estimate::default(),
                Some(5 * GIB),
            )
        };
        let device = |suffix, total, segments| PoolView {
            kind: PoolKind::Device,
            device: Some(named_gpu(suffix)),
            total_bytes: Some(total),
            segments,
            over_total_bytes: 0,
        };

        assert_eq!(
            node_pools(
                "rtx-4090+rtx-4080",
                &[on_gpu(&gpu, 5 * GIB)],
                false,
                &[ready]
            ),
            vec![
                device("4090", 24 * GIB, vec![other(RESERVED)]),
                device(
                    "4080",
                    16 * GIB,
                    vec![measured("model", 5 * GIB), other(RESERVED)]
                ),
                host_pool(64 * GIB, vec![other(4 * GIB)]),
            ]
        );
        assert_eq!(
            node_pools("gb10", &[], false, &[]),
            vec![host_pool(GB10_TOTAL, vec![other(GB10_IDLE_USED)])]
        );
        assert_eq!(
            node_pools("mac-64g", &[], false, &[]),
            vec![host_pool(64 * GIB, vec![other(8 * GIB)])]
        );
    }

    #[test]
    fn host_only_and_mixed_driver_nodes_have_their_pools_and_unknown_architectures_no_size() {
        let host_only = MachineDescription::from_yaml(
            "host-only",
            "platform: linux/amd64\ncpuCount: 8\nhostMemory: { totalBytes: 34359738368, idleUsedBytes: 2147483648 }\n",
        )
        .expect("host-only machine");
        let gpu = |id: &str, driver: &str, total: u64| {
            format!(
                "  - {{ id: {id}, vendor: nvidia, model: GPU, driverVersion: \"{driver}\", memory: {{ architecture: dedicated, totalBytes: {total}, reservedBytes: 0 }} }}\n"
            )
        };
        let mixed_drivers = MachineDescription::from_yaml(
            "mixed-drivers",
            &format!(
                "platform: linux/amd64\ncpuCount: 8\nhostMemory: {{ totalBytes: 34359738368, idleUsedBytes: 2147483648 }}\ngpus:\n{}{}",
                gpu("GPU-a", "580.173.02", 24 * GIB),
                gpu("GPU-b", "575.64.05", 16 * GIB),
            ),
        )
        .expect("mixed-driver machine");
        let pools = |machine: &MachineDescription| {
            NodeCapacity::for_node(&reading(machine, &[], false), &[])
                .pools
                .into_iter()
                .map(|pool| (pool.pool.kind, pool.pool.total_bytes))
                .collect::<Vec<_>>()
        };

        assert_eq!(pools(&host_only), vec![(PoolKind::Host, Some(32 * GIB))]);
        // Drivers that disagree leave the node's targets unavailable, not its pools.
        assert_eq!(
            pools(&mixed_drivers),
            vec![
                (PoolKind::Device, Some(24 * GIB)),
                (PoolKind::Device, Some(16 * GIB)),
                (PoolKind::Host, Some(32 * GIB)),
            ]
        );
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a", ["GPU-a"])],
            [],
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-a",
                &mixed_drivers
                    .render(&[], SystemTime::now())
                    .expect("rendered snapshot"),
                false,
            )],
            Timing::test().node_control.capability_max_age,
            SystemTime::now(),
        );
        assert_eq!(
            inventory.for_target("target-a").expect("target").memory,
            CapabilityObservation::Unavailable
        );

        // A device that reports no classifiable architecture may share the host's memory, so its
        // pool has no size.
        let mut snapshot = mixed_drivers
            .render(&[], SystemTime::now())
            .expect("rendered snapshot");
        let Some(dotcore_proto::dotcore::observation::v1::accelerator_observation::Result::Facts(
            set,
        )) = snapshot.accelerators[0].result.as_mut()
        else {
            unreachable!("rendered NVIDIA facts");
        };
        set.facts[1].memory_architecture = 0;
        let unclassified = NodeCapacity::for_node(
            &NodeMemoryObservation::from_hardware_snapshot("node-a", &snapshot, false).reading,
            &[],
        );
        assert_eq!(unclassified.pools[1].pool.total_bytes, None);
        assert_eq!(unclassified.pools[1].pool.segments, Vec::new());
    }

    #[test]
    fn claims_past_the_total_draw_over_it_and_a_stale_node_keeps_totals_and_claims_only() {
        let gpu = gpu_of("rtx-5090", 0);
        let starting = Workload {
            phase: WorkloadPhase::Starting,
            ..workload(
                WorkloadKey::inference("starting"),
                &gpu,
                Estimate {
                    accelerator_bytes: Some(40 * GIB),
                    host_bytes: Some(GIB),
                },
                Some(4 * GIB),
            )
        };
        let ready = Workload {
            phase: WorkloadPhase::Ready,
            ..workload(
                WorkloadKey::inference("ready"),
                &gpu,
                Estimate::default(),
                Some(2 * GIB),
            )
        };
        let workloads = [starting, ready];
        let running = [on_gpu(&gpu, 4 * GIB), on_gpu(&gpu, 2 * GIB)];

        let fresh =
            NodeCapacity::for_node(&reading(&named("rtx-5090"), &running, false), &workloads);
        assert_eq!(
            fresh.pools[0].pool,
            PoolView {
                kind: PoolKind::Device,
                device: Some(named_gpu("5090")),
                total_bytes: Some(RTX_5090_TOTAL),
                segments: vec![
                    measured("starting", 4 * GIB),
                    claimed("starting", 36 * GIB),
                    measured("ready", 2 * GIB),
                    other(RESERVED),
                ],
                over_total_bytes: 42 * GIB + RESERVED - RTX_5090_TOTAL,
            }
        );
        assert_eq!(
            fresh.pools[1].pool,
            host_pool(64 * GIB, vec![claimed("starting", GIB), other(4 * GIB)])
        );

        // Stale, nothing measured is drawn: the totals and the claims stay.
        let stale =
            NodeCapacity::for_node(&reading(&named("rtx-5090"), &running, true), &workloads);
        assert_eq!(
            stale
                .pools
                .iter()
                .map(|pool| pool.pool.clone())
                .collect::<Vec<_>>(),
            vec![
                PoolView {
                    kind: PoolKind::Device,
                    device: Some(named_gpu("5090")),
                    total_bytes: Some(RTX_5090_TOTAL),
                    segments: vec![claimed("starting", 40 * GIB)],
                    over_total_bytes: 0,
                },
                host_pool(64 * GIB, vec![claimed("starting", GIB)]),
            ]
        );
        assert_eq!(
            stale.pools[0].unsized_workloads,
            vec![WorkloadKey::inference("ready")]
        );
        assert_eq!(
            NodeCapacity::for_node(&CapabilityObservation::Unavailable, &workloads),
            NodeCapacity::default()
        );
    }

    #[test]
    fn a_workload_expected_to_hold_memory_where_it_holds_none_is_unsized_there() {
        let gpu = gpu_of("rtx-5090", 0);
        let unestimated = Workload {
            phase: WorkloadPhase::Ready,
            ..workload(
                WorkloadKey::inference("unestimated"),
                &gpu,
                Estimate::default(),
                None,
            )
        };
        let host_estimate = Workload {
            phase: WorkloadPhase::Ready,
            ..workload(
                WorkloadKey::inference("host estimate"),
                &gpu,
                Estimate {
                    accelerator_bytes: None,
                    host_bytes: Some(GIB),
                },
                Some(GIB),
            )
        };
        let unread = Workload {
            phase: WorkloadPhase::Other,
            ..workload(
                WorkloadKey::applications("unread"),
                &gpu,
                Estimate::default(),
                None,
            )
        };
        let not_applicable = Workload {
            expects_use: false,
            ..unread.clone()
        };
        let not_applicable = Workload {
            key: WorkloadKey::applications("not applicable"),
            ..not_applicable
        };

        let node = NodeCapacity::for_node(
            &reading(&named("rtx-5090"), &[on_gpu(&gpu, GIB)], false),
            &[unestimated, host_estimate, unread, not_applicable],
        );
        assert_eq!(
            node.pools
                .iter()
                .map(|pool| pool.unsized_workloads.clone())
                .collect::<Vec<_>>(),
            vec![
                vec![
                    WorkloadKey::inference("unestimated"),
                    WorkloadKey::applications("unread"),
                ],
                Vec::new(),
            ]
        );
    }

    #[test]
    fn a_nodes_pools_that_a_target_draws_from_read_as_that_targets_preview() {
        for machine in MachineDescription::named_machines().chain(["rtx-4090+rtx-4080"]) {
            let description = named(machine);
            for (index, gpu) in description.gpu_ids().enumerate() {
                let gpu = gpu.to_owned();
                let other_gpu = description.gpu_ids().last().expect("a GPU").to_owned();
                let memory_architecture = architecture_of(&description, &gpu);
                let estimate = Estimate {
                    accelerator_bytes: Some(6 * GIB),
                    host_bytes: Some(GIB),
                };
                let workloads = vec![
                    Workload {
                        phase: WorkloadPhase::Starting,
                        memory_architecture,
                        ..workload(
                            WorkloadKey::inference("starting"),
                            &gpu,
                            estimate,
                            Some(GIB),
                        )
                    },
                    Workload {
                        phase: WorkloadPhase::Ready,
                        memory_architecture,
                        ..workload(
                            WorkloadKey::inference("ready"),
                            &other_gpu,
                            estimate,
                            Some(2 * GIB),
                        )
                    },
                    Workload {
                        phase: WorkloadPhase::Other,
                        memory_architecture,
                        ..workload(
                            WorkloadKey::applications("app"),
                            &gpu,
                            Estimate::default(),
                            Some(GIB),
                        )
                    },
                ];
                let running = [
                    on_gpu(&gpu, GIB),
                    on_gpu(&other_gpu, 2 * GIB),
                    on_gpu(&gpu, GIB),
                ];
                let snapshot = description
                    .render(&running, SystemTime::now())
                    .expect("rendered snapshot");
                let inventory = TargetCapabilityReconciler::reconcile(
                    [("target-a", "node-a", [gpu.clone()])],
                    [],
                    [NodeAcceleratorObservation::from_hardware_snapshot(
                        "node-a", &snapshot, false,
                    )],
                    Timing::test().node_control.capability_max_age,
                    SystemTime::now(),
                );
                let capabilities = inventory.for_target("target-a").expect("target");
                let target = TargetCapacity::for_target(
                    memory_architecture,
                    &NodeHardware::observed(&capabilities.memory),
                    capabilities.accelerator_device_ids.as_deref(),
                )
                .with_workloads(&workloads);
                let node = NodeCapacity::for_node(
                    &NodeMemoryObservation::from_hardware_snapshot("node-a", &snapshot, false)
                        .reading,
                    &workloads,
                );

                let on_target = node
                    .pools
                    .into_iter()
                    .map(|pool| pool.pool)
                    .filter(|pool| match &pool.device {
                        Some(device) => device.id == gpu,
                        None => true,
                    })
                    .collect::<Vec<_>>();
                let previewed = preview(None, &target, None)
                    .pools
                    .into_iter()
                    .map(|pool| pool.pool)
                    .collect::<Vec<_>>();
                assert_eq!(on_target, previewed, "{machine} GPU {index}");
            }
        }
    }

    /// The machine `spec` describes: named GPUs joined by `+` on the simulated host, or one named
    /// machine.
    fn named(spec: &str) -> MachineDescription {
        let choices: Vec<_> = spec.split('+').map(str::to_owned).collect();
        MachineDescription::simulated(&choices, "amd64").expect("simulated machine")
    }

    /// The id of GPU `index` of a described machine.
    fn gpu_of(machine: &str, index: usize) -> String {
        named(machine)
            .gpu_ids()
            .nth(index)
            .expect("the GPU")
            .to_owned()
    }

    /// The memory architecture of a machine's GPU.
    fn architecture_of(machine: &MachineDescription, gpu: &str) -> MemoryArchitecture {
        let snapshot = machine
            .render(&[], SystemTime::now())
            .expect("rendered snapshot");
        let CapabilityObservation::Available(reading) =
            NodeMemoryObservation::from_hardware_snapshot("node-a", &snapshot, false).reading
        else {
            panic!("available reading");
        };
        reading
            .devices()
            .find(|device| device.memory.device_id == gpu)
            .and_then(|device| device.memory_architecture)
            .expect("classified GPU")
    }

    /// The memory reading of a machine running `running`, stale or fresh.
    fn reading(
        machine: &MachineDescription,
        running: &[MachineWorkload],
        stale: bool,
    ) -> CapabilityObservation<dotcore_site::node::target_inventory::NodeMemoryReading> {
        let snapshot = machine
            .render(running, SystemTime::now())
            .expect("rendered snapshot");
        NodeMemoryObservation::from_hardware_snapshot("node-a", &snapshot, stale).reading
    }

    /// The pools a named machine's node draws while `running` takes its memory and `workloads`
    /// hold it.
    fn node_pools(
        machine: &str,
        running: &[MachineWorkload],
        stale: bool,
        workloads: &[Workload],
    ) -> Vec<PoolView> {
        NodeCapacity::for_node(&reading(&named(machine), running, stale), workloads)
            .pools
            .into_iter()
            .map(|NodePool { pool, .. }| pool)
            .collect()
    }

    /// A dedicated workload on `gpu` with the accelerator memory `attributed` to it there.
    fn workload(
        key: WorkloadKey,
        gpu: &str,
        estimate: Estimate,
        attributed: Option<u64>,
    ) -> Workload {
        Workload {
            key,
            phase: WorkloadPhase::Ready,
            expects_use: true,
            memory_architecture: Dedicated,
            device_ids: vec![gpu.to_owned()],
            estimate,
            attributed: attributed.map(|bytes| BTreeMap::from([(gpu.to_owned(), bytes)])),
        }
    }

    /// Memory a workload takes on one GPU of a declared machine.
    fn on_gpu(gpu: &str, bytes: u64) -> MachineWorkload {
        MachineWorkload {
            gpu_bytes: [(gpu.to_owned(), bytes)].into(),
            host_bytes: 0,
        }
    }

    /// A host pool of `total` bytes drawing `segments`.
    fn host_pool(total: u64, segments: Vec<Segment>) -> PoolView {
        PoolView {
            kind: PoolKind::Host,
            device: None,
            total_bytes: Some(total),
            segments,
            over_total_bytes: 0,
        }
    }

    /// The GPU of a named machine whose id ends in `suffix`, as its pool names it.
    fn named_gpu(suffix: &str) -> PoolDevice {
        let (model, vendor_index) = match suffix {
            "5090" => ("NVIDIA GeForce RTX 5090", 0),
            "4090" => ("NVIDIA GeForce RTX 4090", 0),
            "4080" => ("NVIDIA GeForce RTX 4080", 1),
            other => panic!("no named GPU {other}"),
        };
        PoolDevice {
            id: format!("GPU-00000000-0000-0000-0000-00000000{suffix}"),
            model: model.to_owned(),
            vendor_index: Some(vendor_index),
        }
    }

    fn measured(id: &str, bytes: u64) -> Segment {
        segment(SegmentSource::Measured, WorkloadKey::inference(id), bytes)
    }

    fn claimed(id: &str, bytes: u64) -> Segment {
        segment(SegmentSource::Claimed, WorkloadKey::inference(id), bytes)
    }

    fn segment(source: SegmentSource, key: WorkloadKey, bytes: u64) -> Segment {
        Segment {
            source,
            workload: Some(key),
            bytes,
        }
    }

    fn other(bytes: u64) -> Segment {
        Segment {
            source: SegmentSource::Other,
            workload: None,
            bytes,
        }
    }
}
