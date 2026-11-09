use std::{collections::BTreeMap, time::SystemTime};

use dotcore_domain::MemoryArchitecture::{self, Dedicated, Unified};
use dotcore_node_hardware::{MachineDescription, MachineWorkload};
use dotcore_proto::dotcore::{
    catalog::v1::HardwareRequirements,
    deployment::v1::MemoryFit::{self, Exceeds, Fits, Unknown},
    observation::v1::{
        AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
        AcceleratorObservation, AcceleratorVendor, HardwareSnapshot, HostFacts, HostObservation,
        ObservationUnavailable, accelerator_observation, host_observation,
    },
};
use dotcore_site::node::target_inventory::{
    CapabilityObservation, NodeAcceleratorObservation, TargetCapabilityReconciler,
};

use super::evaluation::Preview;
use super::{
    DeviceMemory, Estimate, HostMemory, NodeHardware, Occupancy, Policy, Pool, PoolDevice,
    PoolKind, PoolPreview, PoolView, Segment, SegmentSource,
    StartingTotalContext::{LargestFit, ProfileDefault},
    TargetCapacity, Workload, WorkloadKey, WorkloadPhase, combined_minimums, demand, fit,
    largest_total_context, preview, starting_total_context,
};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const POOL: u64 = 24_000;

/// Host memory beside a synthetic dedicated device, idle and large enough for every host estimate
/// the synthetic cases make.
const HOST: u64 = 8 * POOL;

/// Driver-reserved memory of each named NVIDIA GPU at idle.
const RESERVED: u64 = 497 * MIB;

/// Expected answers for the target on one GPU of a named machine under one occupancy.
struct MachineCase {
    machine: &'static str,
    gpu: usize,
    occupancy: &'static str,
    pools: Vec<Pool>,
    fit_on_free: MemoryFit,
    fit_on_total: MemoryFit,
    largest_on_free: Option<u64>,
    largest_on_total: Option<u64>,
}

#[test]
fn named_machines_give_exact_pools_verdicts_and_largest_fits_under_each_occupancy() {
    const RTX_5090_TOTAL: u64 = 32_607 * MIB;
    const RTX_PRO_6000_TOTAL: u64 = 97_887 * MIB;
    const RTX_4090_TOTAL: u64 = 24 * GIB;
    const RTX_4080_TOTAL: u64 = 16 * GIB;
    const GB10_TOTAL: u64 = 130_596_048_896;
    const GB10_IDLE_USED: u64 = 7_938_146_304;
    const MAC_TOTAL: u64 = 64 * GIB;
    let device = |id: &str, total, used| Pool {
        kind: PoolKind::Device,
        device: Some(named_gpu(id)),
        total_bytes: Some(total),
        occupancy: Occupancy {
            measured_bytes: Some(used),
            ..Occupancy::default()
        },
    };
    let host = |total, used| Pool {
        kind: PoolKind::Host,
        device: None,
        ..device("5090", total, used)
    };
    let case = |machine, gpu, occupancy, pools, fits: [MemoryFit; 2], largest: [Option<u64>; 2]| {
        MachineCase {
            machine,
            gpu,
            occupancy,
            pools,
            fit_on_free: fits[0],
            fit_on_total: fits[1],
            largest_on_free: largest[0],
            largest_on_total: largest[1],
        }
    };
    let cases = [
        // One RTX 5090: the model and the desktop's GPU use count on the GPU, the desktop's host
        // use on the host.
        case(
            "rtx-5090",
            0,
            "idle",
            vec![
                device("5090", RTX_5090_TOTAL, RESERVED),
                host(64 * GIB, 4 * GIB),
            ],
            [Fits, Fits],
            [Some(222_527), Some(229_112)],
        ),
        case(
            "rtx-5090",
            0,
            "one running model",
            vec![
                device("5090", RTX_5090_TOTAL, RESERVED + 10 * GIB),
                host(64 * GIB, 4 * GIB),
            ],
            [Fits, Fits],
            [Some(140_607), Some(229_112)],
        ),
        case(
            "rtx-5090",
            0,
            "busy desktop",
            vec![
                device("5090", RTX_5090_TOTAL, RESERVED + 12 * GIB),
                host(64 * GIB, 12 * GIB),
            ],
            [Exceeds, Fits],
            [Some(124_223), Some(229_112)],
        ),
        case(
            "rtx-pro-6000",
            0,
            "idle",
            vec![
                device("6000", RTX_PRO_6000_TOTAL, RESERVED),
                host(64 * GIB, 4 * GIB),
            ],
            [Fits, Fits],
            [Some(739_545), Some(751_352)],
        ),
        case(
            "rtx-pro-6000",
            0,
            "one running model",
            vec![
                device("6000", RTX_PRO_6000_TOTAL, RESERVED + 10 * GIB),
                host(64 * GIB, 4 * GIB),
            ],
            [Fits, Fits],
            [Some(657_625), Some(751_352)],
        ),
        case(
            "rtx-pro-6000",
            0,
            "busy desktop",
            vec![
                device("6000", RTX_PRO_6000_TOTAL, RESERVED + 12 * GIB),
                host(64 * GIB, 12 * GIB),
            ],
            [Fits, Fits],
            [Some(641_241), Some(751_352)],
        ),
        // GB10 and the Mac: every byte, GPU or host, counts in the host pool.
        case(
            "gb10",
            0,
            "idle",
            vec![host(GB10_TOTAL, GB10_IDLE_USED)],
            [Fits, Fits],
            [Some(885_905), Some(956_432)],
        ),
        case(
            "gb10",
            0,
            "one running model",
            vec![host(GB10_TOTAL, GB10_IDLE_USED + 10 * GIB)],
            [Fits, Fits],
            [Some(803_985), Some(956_432)],
        ),
        case(
            "gb10",
            0,
            "busy desktop",
            vec![host(GB10_TOTAL, GB10_IDLE_USED + 20 * GIB)],
            [Fits, Fits],
            [Some(722_065), Some(956_432)],
        ),
        case(
            "mac-64g",
            0,
            "idle",
            vec![host(MAC_TOTAL, 8 * GIB)],
            [Fits, Fits],
            [Some(413_573), Some(484_352)],
        ),
        case(
            "mac-64g",
            0,
            "one running model",
            vec![host(MAC_TOTAL, 18 * GIB)],
            [Fits, Fits],
            [Some(331_653), Some(484_352)],
        ),
        case(
            "mac-64g",
            0,
            "busy desktop",
            vec![host(MAC_TOTAL, 28 * GIB)],
            [Fits, Fits],
            [Some(249_733), Some(484_352)],
        ),
        // Two GPUs of different sizes: each target is judged on its own GPU, so what runs on the
        // 24 GiB GPU never moves the 16 GiB target's answers.
        case(
            "rtx-4090+rtx-4080",
            0,
            "idle",
            vec![
                device("4090", RTX_4090_TOTAL, RESERVED),
                host(64 * GIB, 4 * GIB),
            ],
            [Fits, Fits],
            [Some(158_921), Some(164_864)],
        ),
        case(
            "rtx-4090+rtx-4080",
            0,
            "one running model",
            vec![
                device("4090", RTX_4090_TOTAL, RESERVED + 10 * GIB),
                host(64 * GIB, 4 * GIB),
            ],
            [Exceeds, Fits],
            [Some(77_001), Some(164_864)],
        ),
        case(
            "rtx-4090+rtx-4080",
            0,
            "busy desktop",
            vec![
                device("4090", RTX_4090_TOTAL, RESERVED + 12 * GIB),
                host(64 * GIB, 12 * GIB),
            ],
            [Exceeds, Fits],
            [Some(60_617), Some(164_864)],
        ),
        case(
            "rtx-4090+rtx-4080",
            1,
            "idle",
            vec![
                device("4080", RTX_4080_TOTAL, RESERVED),
                host(64 * GIB, 4 * GIB),
            ],
            [Exceeds, Exceeds],
            [Some(94_041), Some(99_328)],
        ),
        case(
            "rtx-4090+rtx-4080",
            1,
            "busy desktop",
            vec![
                device("4080", RTX_4080_TOTAL, RESERVED),
                host(64 * GIB, 12 * GIB),
            ],
            [Exceeds, Exceeds],
            [Some(94_041), Some(99_328)],
        ),
    ];

    // 20 GiB of accelerator memory and 2 GiB of host memory; for the largest fit, 4 GiB at the
    // smallest 1024-token total plus 128 KiB per token, and 1 GiB of host memory.
    let estimate = Estimate {
        accelerator_bytes: Some(20 * GIB),
        host_bytes: Some(2 * GIB),
    };
    let at = |tokens: u64| Estimate {
        accelerator_bytes: Some(4 * GIB + (tokens - 1024) * 128 * 1024),
        host_bytes: Some(GIB),
    };
    for case in cases {
        let target = machine_capacity(case.machine, case.gpu, case.occupancy);
        let label = format!("{} GPU {} {}", case.machine, case.gpu, case.occupancy);
        assert_eq!(target.pools, case.pools, "{label}");

        let demand_of = |estimate: Estimate| demand(&estimate, &target).expect("demand");
        let judge = |policy| fit(Some(&demand_of(estimate)), &target, None, policy);
        let largest = |policy| {
            largest_total_context(
                &target,
                None,
                policy,
                1024,
                1_048_576,
                &demand_of(at(1024)),
                &demand_of(at(1025)),
            )
        };
        assert_eq!(
            (judge(Policy::FREE_NOW), judge(Policy::TOTAL)),
            (case.fit_on_free, case.fit_on_total),
            "{label}"
        );
        assert_eq!(
            (largest(Policy::FREE_NOW), largest(Policy::TOTAL)),
            (case.largest_on_free, case.largest_on_total),
            "{label}"
        );
    }
}

#[test]
fn dedicated_demand_holds_the_accelerator_estimate_and_unified_demand_adds_the_host_estimate() {
    let estimate = Estimate {
        accelerator_bytes: Some(20 * GIB),
        host_bytes: Some(2 * GIB),
    };
    for (machine, expected) in [
        ("rtx-5090", vec![20 * GIB, 2 * GIB]),
        ("rtx-4090+rtx-4080", vec![20 * GIB, 2 * GIB]),
        ("gb10", vec![22 * GIB]),
        ("mac-64g", vec![22 * GIB]),
    ] {
        let target = machine_capacity(machine, 0, "idle");
        assert_eq!(
            demand(&estimate, &target).map(|demand| demand.bytes().to_vec()),
            Some(expected),
            "{machine}"
        );
        assert_eq!(
            demand(
                &Estimate {
                    accelerator_bytes: None,
                    ..estimate
                },
                &target
            ),
            None,
            "{machine}: no accelerator estimate, no demand"
        );
    }
}

#[test]
fn a_target_holding_both_mixed_gpus_splits_its_demand_and_divides_by_the_smaller_gpu() {
    let description = described("rtx-4090+rtx-4080");
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let target = snapshot_capacity(&snapshot, description.gpu_ids().collect());
    let estimate = Estimate {
        accelerator_bytes: Some(20 * GIB + 1),
        host_bytes: Some(2 * GIB),
    };

    assert_eq!(
        demand(&estimate, &target).map(|demand| demand.bytes().to_vec()),
        Some(vec![10 * GIB + 1, 10 * GIB, 2 * GIB])
    );
    assert_eq!(target.accelerator_total_bytes(), Some(16 * GIB));
    assert_eq!(
        fit(
            demand(&estimate, &target).as_ref(),
            &target,
            None,
            Policy::FREE_NOW
        ),
        Fits
    );
}

#[test]
fn a_dedicated_target_whose_gpus_dont_resolve_reads_unknown() {
    let snapshot = described("rtx-4090+rtx-4080")
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let estimate = Estimate {
        accelerator_bytes: Some(GIB),
        host_bytes: None,
    };
    for selectors in [vec!["nvidia.com/gpu=2"], Vec::new()] {
        let target = snapshot_capacity(&snapshot, selectors.clone());
        let demand = demand(&estimate, &target);

        assert_eq!(target.accelerator_total_bytes(), None, "{selectors:?}");
        assert_eq!(
            (
                fit(demand.as_ref(), &target, None, Policy::FREE_NOW),
                fit(demand.as_ref(), &target, None, Policy::TOTAL),
            ),
            (Unknown, Unknown),
            "{selectors:?}"
        );
    }
}

#[test]
fn a_starting_workload_counts_its_accelerator_memory_once_and_holds_its_host_estimate_until_ready()
{
    const RTX_5090_FREE: [u64; 2] = [(32_607 - 497 - 3072) * MIB, (65_536 - 4096 - 256) * MIB];
    const GB10_IDLE_USED: u64 = 7_938_146_304;
    const GB10_FREE: u64 = 130_596_048_896 - GB10_IDLE_USED - 3328 * MIB;
    const MAC_FREE: u64 = (65_536 - 8192 - 3328) * MIB;
    let estimate = Estimate {
        accelerator_bytes: Some(3 * GIB),
        host_bytes: Some(256 * MIB),
    };
    for (machine, memory_architecture, running) in [
        ("rtx-5090", Dedicated, RTX_5090_FREE.to_vec()),
        ("gb10", Unified, vec![GB10_FREE]),
        ("mac-64g", Unified, vec![MAC_FREE]),
    ] {
        let description = described(machine);
        let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
        let claim = |attributed: Option<u64>| Workload {
            key: WorkloadKey::inference("starting"),
            expects_use: true,
            phase: WorkloadPhase::Starting,
            memory_architecture,
            device_ids: vec![gpu.clone()],
            estimate,
            attributed: attributed.map(|bytes| BTreeMap::from([(gpu.clone(), bytes)])),
        };
        let loaded = |gpu_bytes| MachineWorkload {
            gpu_bytes: [(gpu.clone(), gpu_bytes)].into(),
            host_bytes: 256 * MIB,
        };
        let free = |workloads: &[MachineWorkload], claims: &[Workload]| {
            let snapshot = description
                .render(workloads, SystemTime::now())
                .expect("rendered snapshot");
            snapshot_capacity(&snapshot, vec![gpu.as_str()])
                .with_workloads(claims)
                .pools
                .iter()
                .map(Pool::free_bytes)
                .collect::<Vec<_>>()
        };
        // While it loads, its host estimate stays claimed beside the host memory it already uses.
        let holding_host = |free: Vec<Option<u64>>| {
            let host = free.len() - 1;
            free.into_iter()
                .enumerate()
                .map(|(index, bytes)| {
                    bytes.map(|bytes| bytes - u64::from(index == host) * 256 * MIB)
                })
                .collect::<Vec<_>>()
        };
        let running = running.into_iter().map(Some).collect::<Vec<_>>();

        // Ready, the workload counts its measured use only.
        assert_eq!(free(&[loaded(3 * GIB)], &[]), running, "{machine} ready");
        // Not loaded yet, its whole estimate is claimed.
        assert_eq!(
            free(&[], &[claim(None)]),
            running,
            "{machine} before loading"
        );
        // Loading, its accelerator claim counts only what its attributed use hasn't reached yet.
        assert_eq!(
            free(&[loaded(GIB)], &[claim(Some(GIB))]),
            holding_host(running.clone()),
            "{machine} partly loaded"
        );
        assert_eq!(
            free(&[loaded(3 * GIB)], &[claim(Some(3 * GIB))]),
            holding_host(running.clone()),
            "{machine} loaded"
        );
        // Measured above its estimate, it counts what it uses.
        assert_eq!(
            free(&[loaded(4 * GIB)], &[claim(Some(4 * GIB))]),
            holding_host(free(&[loaded(4 * GIB)], &[])),
            "{machine} above its estimate"
        );
    }
}

#[test]
fn a_unified_claim_subtracts_all_of_its_attributed_memory_whatever_the_device_id() {
    const GB10_IDLE_USED: u64 = 7_938_146_304;
    const GB10_FREE: u64 = 130_596_048_896 - GB10_IDLE_USED - (3328 + 256) * MIB;
    const MAC_FREE: u64 = (65_536 - 8192 - 3328 - 256) * MIB;
    for (machine, free_while_loading) in [("gb10", GB10_FREE), ("mac-64g", MAC_FREE)] {
        let description = described(machine);
        let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
        let snapshot = description
            .render(
                &[MachineWorkload {
                    gpu_bytes: [(gpu.clone(), GIB)].into(),
                    host_bytes: 256 * MIB,
                }],
                SystemTime::now(),
            )
            .expect("rendered snapshot");
        // A native runtime may attribute memory to no device, and the claim's devices may not
        // resolve.
        let claim = Workload {
            key: WorkloadKey::inference("starting"),
            expects_use: true,
            phase: WorkloadPhase::Starting,
            memory_architecture: Unified,
            device_ids: Vec::new(),
            estimate: Estimate {
                accelerator_bytes: Some(3 * GIB),
                host_bytes: Some(256 * MIB),
            },
            attributed: Some(BTreeMap::from([(String::new(), GIB)])),
        };

        assert_eq!(
            snapshot_capacity(&snapshot, vec![gpu.as_str()])
                .with_workloads(&[claim])
                .pools
                .iter()
                .map(Pool::free_bytes)
                .collect::<Vec<_>>(),
            vec![Some(free_while_loading)],
            "{machine}"
        );
    }
}

#[test]
fn an_unsized_starting_workload_makes_the_pools_it_may_use_unknown() {
    const GPU_1_FREE: u64 = 16 * GIB - 497 * MIB;
    const HOST_FREE: u64 = (60 - 2) * GIB;
    let description = described("rtx-4090+rtx-4080");
    let gpus = description.gpu_ids().map(str::to_owned).collect::<Vec<_>>();
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let unsized_claim = |memory_architecture, device_ids: Vec<String>| Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture,
        device_ids,
        estimate: Estimate {
            accelerator_bytes: None,
            host_bytes: Some(2 * GIB),
        },
        attributed: None,
    };
    let judged = |gpu: &str, claim: &Workload| {
        let target =
            snapshot_capacity(&snapshot, vec![gpu]).with_workloads(std::slice::from_ref(claim));
        let estimate = Estimate {
            accelerator_bytes: Some(GIB),
            host_bytes: None,
        };
        (
            target
                .pools
                .iter()
                .map(Pool::free_bytes)
                .collect::<Vec<_>>(),
            fit(
                demand(&estimate, &target).as_ref(),
                &target,
                None,
                Policy::FREE_NOW,
            ),
        )
    };

    // Its own GPU reads unknown; the other GPU and its host estimate still count.
    let claim = unsized_claim(Dedicated, vec![gpus[0].clone()]);
    assert_eq!(
        judged(&gpus[0], &claim),
        (vec![None, Some(HOST_FREE)], Unknown)
    );
    assert_eq!(
        judged(&gpus[1], &claim),
        (vec![Some(GPU_1_FREE), Some(HOST_FREE)], Fits)
    );

    // On unified memory its accelerator memory comes from the host pool, which reads unknown.
    let gb10 = described("gb10");
    let gb10_gpu = gb10.gpu_ids().next().expect("a GPU").to_owned();
    let gb10_snapshot = gb10
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    assert_eq!(
        snapshot_capacity(&gb10_snapshot, vec![gb10_gpu.as_str()])
            .with_workloads(&[unsized_claim(Unified, vec![gb10_gpu.clone()])])
            .pools
            .iter()
            .map(Pool::free_bytes)
            .collect::<Vec<_>>(),
        vec![None]
    );
}

#[test]
fn a_starting_workload_with_an_unknown_claim_keeps_its_measured_use() {
    let description = described("rtx-5090");
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let snapshot = description
        .render(
            &[MachineWorkload {
                gpu_bytes: [(gpu.clone(), 3 * GIB)].into(),
                host_bytes: 0,
            }],
            SystemTime::now(),
        )
        .expect("rendered snapshot");
    let starting = |device_ids: Vec<String>| Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture: Dedicated,
        device_ids,
        estimate: Estimate {
            accelerator_bytes: None,
            host_bytes: None,
        },
        attributed: Some(BTreeMap::from([(gpu.clone(), 3 * GIB)])),
    };

    // Without an accelerator estimate, or with devices that don't resolve, its claim on the GPU is
    // unknown, but the use attributed to it there is still its own.
    for device_ids in [vec![gpu.clone()], Vec::new()] {
        let gpu_pool = &snapshot_capacity(&snapshot, vec![gpu.as_str()])
            .with_workloads(&[starting(device_ids.clone())])
            .pools[0];
        assert_eq!(gpu_pool.free_bytes(), None, "{device_ids:?}");
        assert_eq!(
            gpu_pool.occupancy.workloads,
            vec![segment(SegmentSource::Measured, "starting", 3 * GIB)],
            "{device_ids:?}"
        );
        assert_eq!(
            gpu_pool.occupancy.other_bytes(),
            Some(RESERVED),
            "{device_ids:?}"
        );
    }
}

#[test]
fn a_starting_workload_whose_gpus_dont_resolve_makes_its_nodes_gpu_pools_unknown() {
    const HOST_FREE: u64 = (60 - 2) * GIB;
    let description = described("rtx-4090+rtx-4080");
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let claim = Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture: Dedicated,
        device_ids: Vec::new(),
        estimate: Estimate {
            accelerator_bytes: Some(10 * GIB),
            host_bytes: Some(2 * GIB),
        },
        attributed: None,
    };

    // It could be on either GPU, so neither target's GPU is judged; its host estimate counts.
    for gpu in description.gpu_ids() {
        assert_eq!(
            snapshot_capacity(&snapshot, vec![gpu])
                .with_workloads(std::slice::from_ref(&claim))
                .pools
                .iter()
                .map(Pool::free_bytes)
                .collect::<Vec<_>>(),
            vec![None, Some(HOST_FREE)],
            "{gpu}"
        );
    }
}

#[test]
fn attribution_newer_than_the_gpu_reading_subtracts_at_most_its_measured_use() {
    const TOTAL: u64 = 32_607 * MIB;
    let description = described("rtx-5090");
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let claim = |estimate_bytes: u64, attributed: u64| Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture: Dedicated,
        device_ids: vec![gpu.clone()],
        estimate: Estimate {
            accelerator_bytes: Some(estimate_bytes),
            host_bytes: None,
        },
        attributed: Some(BTreeMap::from([(gpu.clone(), attributed)])),
    };
    let gpu_free = |gpu_bytes: u64, claims: &[Workload]| {
        let workloads = (gpu_bytes > 0)
            .then(|| MachineWorkload {
                gpu_bytes: [(gpu.clone(), gpu_bytes)].into(),
                host_bytes: 0,
            })
            .into_iter()
            .collect::<Vec<_>>();
        let snapshot = description
            .render(&workloads, SystemTime::now())
            .expect("rendered snapshot");
        snapshot_capacity(&snapshot, vec![gpu.as_str()])
            .with_workloads(claims)
            .pools[0]
            .free_bytes()
    };

    // The GPU reading shows only the driver's 497 MiB, older than the 3 GiB attributed: the claim
    // still takes the 3 GiB the reading doesn't show.
    assert_eq!(
        gpu_free(0, &[claim(3 * GIB, 3 * GIB)]),
        Some(TOTAL - 3 * GIB)
    );
    // Two loading workloads subtract together at most the GPU's 1 GiB and 497 MiB measured use.
    assert_eq!(
        gpu_free(GIB, &[claim(GIB, GIB), claim(3 * GIB, 3 * GIB)]),
        Some(TOTAL - 4 * GIB)
    );
}

#[test]
fn a_claim_counts_only_in_the_pools_it_shares_with_the_target() {
    let description = described("rtx-4090+rtx-4080");
    let gpus = description.gpu_ids().map(str::to_owned).collect::<Vec<_>>();
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let claim = Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture: Dedicated,
        device_ids: vec![gpus[0].clone()],
        estimate: Estimate {
            accelerator_bytes: Some(10 * GIB),
            host_bytes: Some(2 * GIB),
        },
        attributed: None,
    };
    let claimed = |gpu: &str| {
        snapshot_capacity(&snapshot, vec![gpu])
            .with_workloads(std::slice::from_ref(&claim))
            .pools
            .iter()
            .map(|pool| pool.occupancy.claimed_bytes)
            .collect::<Vec<_>>()
    };

    // The 24 GiB GPU's target sees the claim on its GPU and the host; the 16 GiB one on the host.
    assert_eq!(claimed(&gpus[0]), vec![Some(10 * GIB), Some(2 * GIB)]);
    assert_eq!(claimed(&gpus[1]), vec![Some(0), Some(2 * GIB)]);
}

#[test]
fn a_preview_draws_each_pool_with_the_verdicts_the_evaluator_gives_on_named_machines() {
    const RTX_5090_TOTAL: u64 = 32_607 * MIB;
    // The total less its 1% headroom, rounded down to whole bytes.
    const RTX_5090_LINE: u64 = 33_849_008_456;
    const HOST_64G_LINE: u64 = 68_032_281_969;
    const GB10_TOTAL: u64 = 130_596_048_896;
    const GB10_IDLE_USED: u64 = 7_938_146_304;
    const GB10_LINE: u64 = 129_290_088_408;
    let rtx_5090 = named_gpu("5090");
    let estimate = |accelerator_bytes, host_bytes| Estimate {
        accelerator_bytes: Some(accelerator_bytes),
        host_bytes: Some(host_bytes),
    };
    let ready = |gpu: &str, memory_architecture, estimate, attributed: Option<u64>| Workload {
        key: WorkloadKey::inference("running"),
        expects_use: true,
        phase: WorkloadPhase::Ready,
        memory_architecture,
        device_ids: vec![gpu.to_owned()],
        estimate,
        attributed: attributed.map(|bytes| BTreeMap::from([(gpu.to_owned(), bytes)])),
    };
    let device_pool = |segments, free_memory_fit, over_by_bytes, beyond_total| PoolPreview {
        pool: PoolView {
            kind: PoolKind::Device,
            device: Some(rtx_5090.clone()),
            total_bytes: Some(RTX_5090_TOTAL),
            segments,
            over_total_bytes: if beyond_total { over_by_bytes } else { 0 },
        },
        fit_line_bytes: Some(RTX_5090_LINE),
        free_memory_fit,
        over_by_bytes,
        beyond_total,
    };
    let host_pool = |total, line, segments, free_memory_fit| PoolPreview {
        pool: PoolView {
            kind: PoolKind::Host,
            device: None,
            total_bytes: Some(total),
            segments,
            over_total_bytes: 0,
        },
        fit_line_bytes: Some(line),
        free_memory_fit,
        over_by_bytes: 0,
        beyond_total: false,
    };
    let rtx_5090_desktop = described("rtx-5090");
    let rtx_5090_gpu = rtx_5090.id.as_str();
    let model = MachineWorkload {
        gpu_bytes: [(rtx_5090_gpu.to_owned(), 10 * GIB)].into(),
        host_bytes: GIB,
    };
    let running = ready(
        rtx_5090_gpu,
        Dedicated,
        estimate(10 * GIB, GIB),
        Some(10 * GIB),
    );
    let gb10 = described("gb10");
    let gb10_gpu = gb10.gpu_ids().next().expect("a GPU").to_owned();
    let mac = described("mac-64g");
    let mac_gpu = mac.gpu_ids().next().expect("a GPU").to_owned();
    let starting = Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        ..ready(&gb10_gpu, Unified, estimate(3 * GIB, 256 * MIB), None)
    };
    let cases = [
        (
            "rtx-5090 idle",
            &rtx_5090_desktop,
            Vec::new(),
            Vec::new(),
            estimate(20 * GIB, GIB),
            vec![
                device_pool(vec![other(497 * MIB), previewed(20 * GIB)], Fits, 0, false),
                host_pool(
                    64 * GIB,
                    HOST_64G_LINE,
                    vec![other(4 * GIB), previewed(GIB)],
                    Fits,
                ),
            ],
        ),
        // The running model's GPU use is attributed to it; its host estimate stands for its host
        // use.
        (
            "rtx-5090 past the total",
            &rtx_5090_desktop,
            vec![model.clone()],
            vec![running.clone()],
            estimate(22 * GIB, 0),
            vec![
                device_pool(
                    vec![
                        segment(SegmentSource::Measured, "running", 10 * GIB),
                        other(497 * MIB),
                        previewed(22 * GIB),
                    ],
                    Exceeds,
                    32 * GIB + 497 * MIB - RTX_5090_TOTAL,
                    true,
                ),
                host_pool(
                    64 * GIB,
                    HOST_64G_LINE,
                    vec![
                        segment(SegmentSource::Estimated, "running", GIB),
                        other(4 * GIB),
                    ],
                    Fits,
                ),
            ],
        ),
        // One byte past the line, the demand still fits under the total but not the headroom.
        (
            "rtx-5090 within the headroom",
            &rtx_5090_desktop,
            vec![model],
            vec![running],
            estimate(RTX_5090_LINE - 10 * GIB - 497 * MIB + 1, 0),
            vec![
                device_pool(
                    vec![
                        segment(SegmentSource::Measured, "running", 10 * GIB),
                        other(497 * MIB),
                        previewed(RTX_5090_LINE - 10 * GIB - 497 * MIB + 1),
                    ],
                    Exceeds,
                    1,
                    false,
                ),
                host_pool(
                    64 * GIB,
                    HOST_64G_LINE,
                    vec![
                        segment(SegmentSource::Estimated, "running", GIB),
                        other(4 * GIB),
                    ],
                    Fits,
                ),
            ],
        ),
        // Nothing of the starting deployment is measured yet: its whole estimate is claimed.
        (
            "gb10 with a starting deployment",
            &gb10,
            Vec::new(),
            vec![starting],
            estimate(20 * GIB, 2 * GIB),
            vec![host_pool(
                GB10_TOTAL,
                GB10_LINE,
                vec![
                    segment(SegmentSource::Claimed, "starting", 3 * GIB + 256 * MIB),
                    other(GB10_IDLE_USED),
                    previewed(22 * GIB),
                ],
                Fits,
            )],
        ),
        // A native runtime attributes nothing: the ready model's estimate stands for its use.
        (
            "mac-64g with an unattributed model",
            &mac,
            vec![MachineWorkload {
                gpu_bytes: [(mac_gpu.clone(), 4 * GIB)].into(),
                host_bytes: 0,
            }],
            vec![ready(&mac_gpu, Unified, estimate(4 * GIB, 0), None)],
            estimate(20 * GIB, 0),
            vec![host_pool(
                64 * GIB,
                HOST_64G_LINE,
                vec![
                    segment(SegmentSource::Estimated, "running", 4 * GIB),
                    other(8 * GIB),
                    previewed(20 * GIB),
                ],
                Fits,
            )],
        ),
    ];
    for (case, machine, running, workloads, estimate, pools) in cases {
        let gpu = machine.gpu_ids().next().expect("a GPU");
        let snapshot = machine
            .render(&running, SystemTime::now())
            .expect("rendered snapshot");
        let target = snapshot_capacity(&snapshot, vec![gpu]).with_workloads(&workloads);
        let demand = demand(&estimate, &target);

        let previewed = preview(demand.as_ref(), &target, None);

        // Its verdict is the evaluator's, and each pool draws its measured and claimed use with
        // the demand added.
        let verdict = fit(demand.as_ref(), &target, None, Policy::FREE_NOW);
        assert_eq!(
            previewed,
            Preview {
                pools,
                free_memory_fit: verdict,
            },
            "{case}"
        );
        let demand = demand.expect("demand");
        for (index, pool) in target.pools.iter().enumerate() {
            let drawn = previewed.pools[index]
                .pool
                .segments
                .iter()
                .map(|segment| segment.bytes)
                .sum::<u64>();
            assert_eq!(
                drawn,
                pool.occupancy.measured_bytes.expect("measured")
                    + pool.occupancy.claimed_bytes.expect("claimed")
                    + demand.bytes()[index],
                "{case}"
            );
        }
    }
}

#[test]
fn a_preview_of_an_unknown_pool_draws_what_is_known_and_judges_nothing() {
    let description = described("rtx-4090+rtx-4080");
    let gpus = description.gpu_ids().map(str::to_owned).collect::<Vec<_>>();
    let snapshot = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    // A starting deployment of unknown size on the GPU leaves its claim there unknown.
    let unsized_workload = Workload {
        key: WorkloadKey::inference("starting"),
        expects_use: true,
        phase: WorkloadPhase::Starting,
        memory_architecture: Dedicated,
        device_ids: vec![gpus[0].clone()],
        estimate: Estimate {
            accelerator_bytes: None,
            host_bytes: Some(2 * GIB),
        },
        attributed: None,
    };
    let target = snapshot_capacity(&snapshot, vec![gpus[0].as_str()])
        .with_workloads(std::slice::from_ref(&unsized_workload));
    let estimate = Estimate {
        accelerator_bytes: Some(GIB),
        host_bytes: None,
    };

    let previewed_on_gpu = preview(demand(&estimate, &target).as_ref(), &target, None);

    assert_eq!(
        previewed_on_gpu.pools[0],
        PoolPreview {
            pool: PoolView {
                kind: PoolKind::Device,
                device: Some(named_gpu("4090")),
                total_bytes: Some(24 * GIB),
                segments: vec![other(497 * MIB), previewed(GIB)],
                over_total_bytes: 0,
            },
            fit_line_bytes: Some(24 * GIB - 257_698_037),
            free_memory_fit: Unknown,
            over_by_bytes: 0,
            beyond_total: false,
        }
    );
    assert_eq!(previewed_on_gpu.free_memory_fit, Unknown);

    // A target whose GPUs don't resolve has one pool of unknown size, drawn empty but for its
    // demand.
    let unresolved = snapshot_capacity(&snapshot, Vec::new());
    assert_eq!(
        preview(demand(&estimate, &unresolved).as_ref(), &unresolved, None).pools[0],
        PoolPreview {
            pool: PoolView {
                kind: PoolKind::Device,
                device: None,
                total_bytes: None,
                segments: vec![previewed(GIB)],
                over_total_bytes: 0,
            },
            fit_line_bytes: None,
            free_memory_fit: Unknown,
            over_by_bytes: 0,
            beyond_total: false,
        }
    );
}

#[test]
fn ready_and_stopping_workloads_split_measured_use_without_changing_what_is_free() {
    const TOTAL: u64 = 32_607 * MIB;
    let description = described("rtx-5090");
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let snapshot = description
        .render(
            &[MachineWorkload {
                gpu_bytes: [(gpu.clone(), 4 * GIB)].into(),
                host_bytes: 0,
            }],
            SystemTime::now(),
        )
        .expect("rendered snapshot");
    let workload = |workload_id: &str, phase, attributed: Option<u64>| Workload {
        key: WorkloadKey::inference(workload_id),
        expects_use: true,
        phase,
        memory_architecture: Dedicated,
        device_ids: vec![gpu.clone()],
        estimate: Estimate {
            accelerator_bytes: Some(3 * GIB),
            host_bytes: None,
        },
        attributed: attributed.map(|bytes| BTreeMap::from([(gpu.clone(), bytes)])),
    };
    // A stopping deployment counts only its attributed use; a ready one without attribution its
    // estimate, capped by what attributed use leaves of the reading, whatever their order.
    let target = snapshot_capacity(&snapshot, vec![gpu.as_str()]).with_workloads(&[
        workload("ready", WorkloadPhase::Ready, None),
        workload("stopping", WorkloadPhase::Other, Some(2 * GIB)),
        workload("unattributed stopping", WorkloadPhase::Other, None),
    ]);

    let gpu_pool = &target.pools[0];
    assert_eq!(gpu_pool.free_bytes(), Some(TOTAL - 4 * GIB - 497 * MIB));
    assert_eq!(
        gpu_pool.occupancy.workloads,
        vec![
            segment(SegmentSource::Estimated, "ready", 2 * GIB + 497 * MIB),
            segment(SegmentSource::Measured, "stopping", 2 * GIB),
        ]
    );
    assert_eq!(gpu_pool.occupancy.other_bytes(), Some(0));
}

#[test]
fn an_application_named_in_a_preview_splits_other_use_without_changing_its_verdicts() {
    const TOTAL: u64 = 32_607 * MIB;
    let description = described("rtx-5090");
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let on_gpu = |bytes| MachineWorkload {
        gpu_bytes: [(gpu.clone(), bytes)].into(),
        host_bytes: 0,
    };
    let snapshot = description
        .render(&[on_gpu(10 * GIB), on_gpu(3 * GIB)], SystemTime::now())
        .expect("rendered snapshot");
    let model = Workload {
        key: WorkloadKey::inference("model"),
        phase: WorkloadPhase::Ready,
        expects_use: true,
        memory_architecture: Dedicated,
        device_ids: vec![gpu.clone()],
        estimate: Estimate::default(),
        attributed: Some(BTreeMap::from([(gpu.clone(), 10 * GIB)])),
    };
    let application = Workload {
        key: WorkloadKey::applications("app"),
        phase: WorkloadPhase::Other,
        attributed: Some(BTreeMap::from([(gpu.clone(), 3 * GIB)])),
        ..model.clone()
    };
    let estimate = Estimate {
        accelerator_bytes: Some(20 * GIB),
        host_bytes: None,
    };
    let previewed_with = |workloads: &[Workload]| {
        let target = snapshot_capacity(&snapshot, vec![gpu.as_str()]).with_workloads(workloads);
        preview(demand(&estimate, &target).as_ref(), &target, None)
    };

    let models_only = previewed_with(std::slice::from_ref(&model));
    let named = previewed_with(&[model, application]);

    assert_eq!(
        models_only.pools[0].pool.segments,
        vec![
            segment(SegmentSource::Measured, "model", 10 * GIB),
            other(3 * GIB + 497 * MIB),
            previewed(20 * GIB),
        ]
    );
    assert_eq!(
        named.pools[0].pool.segments,
        vec![
            segment(SegmentSource::Measured, "model", 10 * GIB),
            Segment {
                source: SegmentSource::Measured,
                workload: Some(WorkloadKey::applications("app")),
                bytes: 3 * GIB,
            },
            other(497 * MIB),
            previewed(20 * GIB),
        ]
    );
    // 33 GiB and 497 MiB are drawn either way, past the GPU's total.
    assert_eq!(named.free_memory_fit, Exceeds);
    assert_eq!(named.pools[0].over_by_bytes, 33 * GIB + 497 * MIB - TOTAL);
    let verdicts = |preview: &Preview| {
        preview
            .pools
            .iter()
            .map(|pool| {
                (
                    pool.free_memory_fit,
                    pool.over_by_bytes,
                    pool.beyond_total,
                    pool.pool.over_total_bytes,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(verdicts(&named), verdicts(&models_only));
    assert_eq!(named.free_memory_fit, models_only.free_memory_fit);
}

#[test]
fn a_deployment_starts_from_the_largest_fit_in_whole_blocks_or_the_default() {
    for (largest, default, expected) in [
        (Some(222_527), Some(32_768), Some(LargestFit(222_208))),
        (Some(2_048), Some(32_768), Some(LargestFit(2_048))),
        (Some(1_024), None, Some(LargestFit(1_024))),
        (Some(1_023), Some(32_768), Some(ProfileDefault(32_768))),
        (Some(1_023), None, None),
        (None, Some(32_768), Some(ProfileDefault(32_768))),
        (None, None, None),
    ] {
        assert_eq!(
            starting_total_context(largest, default),
            expected,
            "{largest:?} {default:?}"
        );
    }
}

#[test]
fn the_largest_total_context_stays_within_the_highest_total_and_needs_a_growing_demand() {
    let target = capacity(Dedicated, Some(POOL), Some(POOL));
    let demand_of = |bytes| {
        demand(
            &Estimate {
                accelerator_bytes: Some(bytes),
                host_bytes: None,
            },
            &target,
        )
        .expect("demand")
    };
    let largest = |first, next| {
        largest_total_context(&target, None, Policy::TOTAL, 1024, 4096, &first, &next)
    };

    assert_eq!(
        largest(demand_of(0), demand_of(0)),
        Some(4096),
        "zero slope"
    );
    assert_eq!(largest(demand_of(0), demand_of(10)), Some(3424));
    assert_eq!(largest(demand_of(POOL), demand_of(POOL + 1)), Some(1024));
    assert_eq!(largest(demand_of(POOL + 1), demand_of(POOL + 2)), None);
    assert_eq!(
        largest(demand_of(10), demand_of(9)),
        None,
        "a shrinking demand"
    );
    assert_eq!(
        largest_total_context(
            &TargetCapacity::default(),
            None,
            Policy::TOTAL,
            1024,
            4096,
            &demand_of(0),
            &demand_of(0)
        ),
        None,
        "no pools"
    );
}

#[test]
fn no_total_context_fits_a_target_below_a_catalog_minimum() {
    let largest = |memory_architecture, minimums: HardwareRequirements| {
        let target = capacity(memory_architecture, Some(POOL), Some(POOL));
        let demand_of = |bytes| {
            demand(
                &Estimate {
                    accelerator_bytes: Some(bytes),
                    host_bytes: None,
                },
                &target,
            )
            .expect("demand")
        };
        [Policy::FREE_NOW, Policy::TOTAL].map(|policy| {
            largest_total_context(
                &target,
                Some(&minimums),
                policy,
                1024,
                4096,
                &demand_of(0),
                &demand_of(1),
            )
        })
    };

    // The estimates alone would fit every total; the minimums decide.
    assert_eq!(
        largest(Dedicated, minimums(POOL, HOST)),
        [Some(4096), Some(4096)]
    );
    assert_eq!(largest(Dedicated, minimums(POOL + 1, 0)), [None, None]);
    assert_eq!(largest(Dedicated, minimums(0, HOST + 1)), [None, None]);
    assert_eq!(
        largest(Unified, minimums(POOL, POOL)),
        [Some(4096), Some(4096)]
    );
    assert_eq!(largest(Unified, minimums(POOL + 1, 0)), [None, None]);
    assert_eq!(largest(Unified, minimums(0, POOL + 1)), [None, None]);
}

#[test]
fn dedicated_memory_compares_the_accelerator_estimate_with_the_per_device_total() {
    assert_eq!(
        on_total(Dedicated, Some(POOL), Some(POOL), None, None),
        Fits
    );
    assert_eq!(
        on_total(Dedicated, Some(POOL), Some(POOL + 1), Some(0), None),
        Exceeds
    );

    // The host estimate lives outside the device: it counts on the host pool only.
    assert_eq!(
        on_total(Dedicated, Some(POOL), Some(POOL), Some(HOST), None),
        Fits
    );
    assert_eq!(
        on_total(Dedicated, Some(POOL), Some(POOL), Some(HOST + 1), None),
        Exceeds
    );
    assert_eq!(
        demand_bytes(Dedicated, Some(10), Some(POOL)),
        Some(vec![10, POOL])
    );
}

#[test]
fn unified_memory_compares_both_estimates_together_with_the_shared_pool() {
    assert_eq!(
        on_total(Unified, Some(POOL), Some(POOL - 10), Some(10), None),
        Fits
    );
    assert_eq!(
        on_total(Unified, Some(POOL), Some(POOL - 10), Some(11), None),
        Exceeds
    );
    assert_eq!(
        demand_bytes(Unified, Some(u64::MAX), Some(1)),
        Some(vec![u64::MAX]),
        "the sum saturates"
    );
}

#[test]
fn a_missing_host_estimate_adds_no_host_footprint() {
    assert_eq!(on_total(Unified, Some(POOL), Some(POOL), None, None), Fits);
    assert_eq!(
        on_total(Unified, Some(POOL), Some(POOL + 1), None, None),
        Exceeds
    );
    assert_eq!(demand_bytes(Unified, Some(10), None), Some(vec![10]));
}

#[test]
fn a_missing_accelerator_estimate_or_total_reads_unknown() {
    assert_eq!(
        on_total(Dedicated, Some(POOL), None, Some(0), None),
        Unknown
    );
    assert_eq!(
        on_total(Unified, Some(POOL), None, Some(POOL + 1), None),
        Unknown
    );
    assert_eq!(demand_bytes(Unified, None, Some(10)), None);
    assert_eq!(on_total(Dedicated, None, Some(0), Some(0), None), Unknown);
    assert_eq!(
        on_total(Unified, None, Some(0), Some(0), Some(&minimums(POOL, POOL))),
        Unknown,
        "a minimum needs a total to compare"
    );
}

#[test]
fn extra_minimums_raise_each_declared_minimum_they_exceed() {
    let declared = minimums(16 * GIB, 64 * GIB);
    assert_eq!(combined_minimums(None, None), None);
    assert_eq!(combined_minimums(Some(&declared), None), Some(declared));
    assert_eq!(combined_minimums(None, Some(&declared)), Some(declared));
    // Each field keeps the larger value; a zero sets none.
    assert_eq!(
        combined_minimums(Some(&declared), Some(&minimums(24 * GIB, 0))),
        Some(minimums(24 * GIB, 64 * GIB))
    );
    assert_eq!(
        combined_minimums(Some(&declared), Some(&minimums(8 * GIB, 96 * GIB))),
        Some(minimums(16 * GIB, 96 * GIB))
    );
}

#[test]
fn a_minimum_above_the_compared_total_exceeds_it() {
    // The per-device minimum compares with the per-device total on dedicated memory, even
    // without an estimate, and the system minimum with the host total.
    assert_eq!(
        on_total(
            Dedicated,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(POOL + 1, 0))
        ),
        Exceeds
    );
    assert_eq!(
        on_total(
            Dedicated,
            Some(POOL),
            None,
            None,
            Some(&minimums(POOL + 1, 0))
        ),
        Exceeds
    );
    assert_eq!(
        on_total(
            Dedicated,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(POOL, HOST))
        ),
        Fits
    );
    assert_eq!(
        on_total(
            Dedicated,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(POOL, HOST + 1))
        ),
        Exceeds
    );

    // Both minimums compare with the shared pool on unified memory.
    assert_eq!(
        on_total(
            Unified,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(POOL + 1, 0))
        ),
        Exceeds
    );
    assert_eq!(
        on_total(
            Unified,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(0, POOL + 1))
        ),
        Exceeds
    );
    assert_eq!(
        on_total(
            Unified,
            Some(POOL),
            Some(1),
            Some(1),
            Some(&minimums(POOL, POOL))
        ),
        Fits
    );
}

#[test]
fn free_memory_fit_compares_with_free_memory_less_a_headroom_of_the_total() {
    let total = 32_607 * MIB;
    let free = 20_000 * MIB;
    // 1% of the total, 326.07 MiB, comes off the free memory.
    assert_eq!(usable_free_bytes(total, free), free - total / 100);
    assert_eq!(usable_free_bytes(total, total / 200), 0, "never below zero");

    for (estimate, expected_on_free, expected_on_total) in [
        (19_600 * MIB, Fits, Fits),
        (19_700 * MIB, Exceeds, Fits),
        (free - total / 100, Fits, Fits),
        (free - total / 100 + 1, Exceeds, Fits),
    ] {
        assert_eq!(
            (
                on_free(
                    Dedicated,
                    Some(total),
                    Some(free),
                    Some(estimate),
                    None,
                    None
                ),
                on_total(Dedicated, Some(total), Some(estimate), None, None),
            ),
            (expected_on_free, expected_on_total),
            "{estimate} bytes"
        );
    }
}

#[test]
fn free_memory_fit_counts_the_host_estimate_on_unified_memory_only() {
    let free = POOL / 2 + POOL / 100;
    assert_eq!(
        on_free(
            Unified,
            Some(POOL),
            Some(free),
            Some(POOL / 2 - 10),
            Some(10),
            None
        ),
        Fits
    );
    assert_eq!(
        on_free(
            Unified,
            Some(POOL),
            Some(free),
            Some(POOL / 2 - 10),
            Some(11),
            None
        ),
        Exceeds
    );
    assert_eq!(
        on_free(
            Dedicated,
            Some(POOL),
            Some(free),
            Some(POOL / 2),
            Some(POOL),
            None
        ),
        Fits
    );
}

#[test]
fn free_memory_fit_is_unknown_without_free_memory_a_total_or_an_estimate() {
    assert_eq!(
        on_free(Dedicated, Some(POOL), None, Some(1), None, None),
        Unknown
    );
    assert_eq!(
        on_free(Dedicated, None, Some(POOL), Some(1), None, None),
        Unknown
    );
    assert_eq!(
        on_free(Dedicated, Some(POOL), Some(POOL), None, None, None),
        Unknown
    );
}

#[test]
fn free_memory_fit_compares_minimums_with_the_total() {
    // A minimum above the total exceeds even with free memory unknown.
    for free in [Some(POOL), None] {
        assert_eq!(
            on_free(
                Dedicated,
                Some(POOL),
                free,
                Some(1),
                None,
                Some(&minimums(POOL + 1, 0))
            ),
            Exceeds,
            "{free:?}"
        );
        assert_eq!(
            on_free(
                Unified,
                Some(POOL),
                free,
                Some(1),
                None,
                Some(&minimums(0, POOL + 1))
            ),
            Exceeds,
            "{free:?}"
        );
    }

    // A minimum within the total but above free memory doesn't exceed by itself.
    assert_eq!(
        on_free(
            Dedicated,
            Some(POOL),
            Some(POOL / 2),
            Some(1),
            None,
            Some(&minimums(POOL, 0))
        ),
        Fits
    );
    assert_eq!(
        on_free(
            Unified,
            Some(POOL),
            Some(POOL / 2),
            Some(1),
            Some(1),
            Some(&minimums(POOL, POOL))
        ),
        Fits
    );
}

#[test]
fn node_snapshot_memory_pool_total_follows_the_memory_architecture() {
    use AcceleratorMemoryArchitecture::{Dedicated, Unified};

    const DEVICE_TOTAL: u64 = 34_190_917_632;
    const SMALLER_TOTAL: u64 = 25_769_803_776;
    const HOST_TOTAL: u64 = 130_596_225_024;
    let total = |devices: &[(AcceleratorMemoryArchitecture, Option<u64>)],
                 host_total,
                 selectors: Vec<&str>| {
        snapshot_capacity(&memory_snapshot(devices, host_total), selectors)
            .accelerator_total_bytes()
    };
    let mixed = [
        (Dedicated, Some(DEVICE_TOTAL)),
        (Dedicated, Some(SMALLER_TOTAL)),
    ];

    assert_eq!(
        total(
            &[(Dedicated, Some(DEVICE_TOTAL))],
            Some(HOST_TOTAL),
            vec!["nvidia.com/gpu=0"]
        ),
        Some(DEVICE_TOTAL)
    );
    assert_eq!(
        total(&mixed, Some(HOST_TOTAL), vec!["nvidia.com/gpu=0"]),
        Some(DEVICE_TOTAL)
    );
    assert_eq!(
        total(&mixed, Some(HOST_TOTAL), vec!["nvidia.com/gpu=1"]),
        Some(SMALLER_TOTAL)
    );
    assert_eq!(
        total(
            &mixed,
            Some(HOST_TOTAL),
            vec!["nvidia.com/gpu=0", "nvidia.com/gpu=1"]
        ),
        Some(SMALLER_TOTAL),
        "the smaller device of the two"
    );
    assert_eq!(
        total(
            &[(Dedicated, None)],
            Some(HOST_TOTAL),
            vec!["nvidia.com/gpu=0"]
        ),
        None
    );
    assert_eq!(
        total(&[(Unified, None)], Some(HOST_TOTAL), Vec::new()),
        Some(HOST_TOTAL)
    );
    assert_eq!(total(&[(Unified, None)], None, Vec::new()), None);
}

#[test]
fn node_snapshot_free_memory_follows_the_memory_architecture() {
    use AcceleratorMemoryArchitecture::{Dedicated, Unified};

    let free = |snapshot: &HardwareSnapshot, selectors: Vec<&str>| {
        snapshot_capacity(snapshot, selectors)
            .accelerator_pools()
            .map(Pool::free_bytes)
            .collect::<Vec<_>>()
    };
    let with_used = |used: &[Option<u64>]| {
        let devices = vec![(Dedicated, Some(32 * GIB)); used.len()];
        let mut snapshot = memory_snapshot(&devices, Some(64 * GIB));
        let Some(accelerator_observation::Result::Facts(set)) =
            snapshot.accelerators[0].result.as_mut()
        else {
            unreachable!("NVIDIA snapshot has facts");
        };
        for (fact, used) in set.facts.iter_mut().zip(used) {
            fact.used_memory_bytes = *used;
        }
        snapshot
    };

    // Each target reads its own device, whatever the other one uses.
    let two = with_used(&[Some(GIB), Some(5 * GIB)]);
    assert_eq!(free(&two, vec!["nvidia.com/gpu=0"]), vec![Some(31 * GIB)]);
    assert_eq!(free(&two, vec!["nvidia.com/gpu=1"]), vec![Some(27 * GIB)]);
    let one_unknown = with_used(&[Some(GIB), None]);
    assert_eq!(
        free(&one_unknown, vec!["nvidia.com/gpu=0"]),
        vec![Some(31 * GIB)]
    );
    assert_eq!(free(&one_unknown, vec!["nvidia.com/gpu=1"]), vec![None]);

    let mut unified = memory_snapshot(&[(Unified, None)], Some(64 * GIB));
    unified.host = Some(HostObservation {
        result: Some(host_observation::Result::Facts(HostFacts {
            total_memory_bytes: 64 * GIB,
            available_memory_bytes: 40 * GIB,
            ..Default::default()
        })),
    });
    assert_eq!(free(&unified, Vec::new()), vec![Some(40 * GIB)]);

    let no_host = memory_snapshot(&[(Unified, None)], None);
    assert_eq!(free(&no_host, Vec::new()), vec![None]);
}

#[test]
fn a_stale_observation_keeps_its_totals_without_measured_use() {
    let description = described("rtx-5090");
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let rendered = description
        .render(&[], SystemTime::now())
        .expect("rendered snapshot");
    let stale = NodeAcceleratorObservation::from_hardware_snapshot("node-a", &rendered, true);
    let inventory = TargetCapabilityReconciler::reconcile(
        [("target-a", "node-a", [gpu.clone()])],
        [],
        [stale],
        dotcore_domain::timing::Timing::production()
            .node_control
            .capability_max_age,
    );
    let capabilities = inventory.for_target("target-a").expect("target");
    assert!(matches!(
        capabilities.memory,
        CapabilityObservation::Stale(_)
    ));

    let unmeasured = |kind, device, total| Pool {
        kind,
        device,
        total_bytes: Some(total),
        occupancy: Occupancy::default(),
    };
    assert_eq!(
        TargetCapacity::for_target(
            Dedicated,
            &NodeHardware::observed(&capabilities.memory),
            capabilities.accelerator_device_ids.as_deref(),
        )
        .pools,
        vec![
            unmeasured(PoolKind::Device, Some(named_gpu("5090")), 32_607 * MIB),
            unmeasured(PoolKind::Host, None, 64 * GIB),
        ]
    );
}

/// The GPU of a named machine whose id ends in `suffix`, as its pool names it.
fn named_gpu(suffix: &str) -> PoolDevice {
    let (model, vendor_index) = match suffix {
        "5090" => ("NVIDIA GeForce RTX 5090", 0),
        "6000" => ("NVIDIA RTX PRO 6000 Blackwell Workstation Edition", 0),
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

/// `bytes` of a workload's segment.
fn segment(source: SegmentSource, workload_id: &str, bytes: u64) -> Segment {
    Segment {
        source,
        workload: Some(WorkloadKey::inference(workload_id)),
        bytes,
    }
}

/// `bytes` of other use.
fn other(bytes: u64) -> Segment {
    Segment {
        source: SegmentSource::Other,
        workload: None,
        bytes,
    }
}

/// `bytes` of the previewed demand.
fn previewed(bytes: u64) -> Segment {
    Segment {
        source: SegmentSource::Previewed,
        workload: None,
        bytes,
    }
}

/// Pools of the target on GPU `gpu` of a named machine with `occupancy` running on its first GPU:
/// nothing, one 10 GiB model, or that model beside a desktop using 2 GiB of GPU and 8 GiB of host
/// memory.
fn machine_capacity(machine: &str, gpu: usize, occupancy: &str) -> TargetCapacity {
    let description = described(machine);
    let target_gpu = description.gpu_ids().nth(gpu).expect("the GPU").to_owned();
    let gpu = description.gpu_ids().next().expect("a GPU").to_owned();
    let model = MachineWorkload {
        gpu_bytes: [(gpu.clone(), 10 * GIB)].into(),
        host_bytes: 0,
    };
    let desktop = MachineWorkload {
        gpu_bytes: [(gpu, 2 * GIB)].into(),
        host_bytes: 8 * GIB,
    };
    let workloads = match occupancy {
        "idle" => vec![],
        "one running model" => vec![model],
        "busy desktop" => vec![model, desktop],
        other => panic!("unknown occupancy {other}"),
    };
    let snapshot = description
        .render(&workloads, SystemTime::now())
        .expect("rendered snapshot");
    snapshot_capacity(&snapshot, vec![target_gpu.as_str()])
}

/// Pools of one target holding the devices `selectors` name on a node that reported `snapshot`,
/// through the site's reconciliation.
fn snapshot_capacity(snapshot: &HardwareSnapshot, selectors: Vec<&str>) -> TargetCapacity {
    let inventory = TargetCapabilityReconciler::reconcile(
        [("target-a", "node-a", selectors)],
        [],
        [NodeAcceleratorObservation::from_hardware_snapshot(
            "node-a", snapshot, false,
        )],
        dotcore_domain::timing::Timing::production()
            .node_control
            .capability_max_age,
    );
    let capabilities = inventory.for_target("target-a").expect("target");
    let CapabilityObservation::Available(memory_architecture) = capabilities.accelerator_memory
    else {
        panic!("classified memory architecture");
    };
    TargetCapacity::for_target(
        memory_architecture,
        &NodeHardware::observed(&capabilities.memory),
        capabilities.accelerator_device_ids.as_deref(),
    )
}

/// Pools of a target holding one device beside an idle host, or a host on unified memory, with
/// `total` and `free` memory.
fn capacity(
    memory_architecture: MemoryArchitecture,
    total: Option<u64>,
    free: Option<u64>,
) -> TargetCapacity {
    let node = match memory_architecture {
        Dedicated => NodeHardware {
            devices: vec![DeviceMemory {
                device: PoolDevice {
                    id: "GPU-a".to_owned(),
                    ..PoolDevice::default()
                },
                total_bytes: total,
                used_bytes: total.zip(free).map(|(total, free)| total - free),
            }],
            host: Some(HostMemory {
                total_bytes: HOST,
                available_bytes: Some(HOST),
            }),
        },
        Unified => NodeHardware {
            devices: Vec::new(),
            host: total.map(|total_bytes| HostMemory {
                total_bytes,
                available_bytes: free,
            }),
        },
    };
    TargetCapacity::for_target(memory_architecture, &node, Some(&["GPU-a".to_owned()]))
}

fn on_total(
    memory_architecture: MemoryArchitecture,
    total: Option<u64>,
    accelerator_bytes: Option<u64>,
    host_bytes: Option<u64>,
    minimums: Option<&HardwareRequirements>,
) -> MemoryFit {
    judge(
        memory_architecture,
        total,
        None,
        accelerator_bytes,
        host_bytes,
        minimums,
        Policy::TOTAL,
    )
}

fn on_free(
    memory_architecture: MemoryArchitecture,
    total: Option<u64>,
    free: Option<u64>,
    accelerator_bytes: Option<u64>,
    host_bytes: Option<u64>,
    minimums: Option<&HardwareRequirements>,
) -> MemoryFit {
    judge(
        memory_architecture,
        total,
        free,
        accelerator_bytes,
        host_bytes,
        minimums,
        Policy::FREE_NOW,
    )
}

fn judge(
    memory_architecture: MemoryArchitecture,
    total: Option<u64>,
    free: Option<u64>,
    accelerator_bytes: Option<u64>,
    host_bytes: Option<u64>,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
) -> MemoryFit {
    let target = capacity(memory_architecture, total, free);
    let demand = demand(
        &Estimate {
            accelerator_bytes,
            host_bytes,
        },
        &target,
    );
    fit(demand.as_ref(), &target, minimums, policy)
}

fn demand_bytes(
    memory_architecture: MemoryArchitecture,
    accelerator_bytes: Option<u64>,
    host_bytes: Option<u64>,
) -> Option<Vec<u64>> {
    demand(
        &Estimate {
            accelerator_bytes,
            host_bytes,
        },
        &capacity(memory_architecture, Some(POOL), None),
    )
    .map(|demand| demand.bytes().to_vec())
}

/// Free memory an estimate may use on a device pool with `total` and `free` memory.
fn usable_free_bytes(total: u64, free: u64) -> u64 {
    Policy::FREE_NOW
        .usable_bytes(&capacity(Dedicated, Some(total), Some(free)).pools[0])
        .expect("known pool")
}

fn minimums(per_device: u64, system: u64) -> HardwareRequirements {
    HardwareRequirements {
        minimum_system_memory_bytes: system,
        minimum_memory_bytes_per_device: per_device,
    }
}

fn memory_snapshot(
    devices: &[(AcceleratorMemoryArchitecture, Option<u64>)],
    host_total: Option<u64>,
) -> HardwareSnapshot {
    HardwareSnapshot {
        observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
            seconds: 1_725_000_000,
            nanos: 0,
        }),
        accelerators: vec![AcceleratorObservation {
            vendor: AcceleratorVendor::Nvidia as i32,
            result: Some(accelerator_observation::Result::Facts(
                AcceleratorFactsSet {
                    facts: devices
                        .iter()
                        .enumerate()
                        .map(|(index, (architecture, total))| AcceleratorFacts {
                            vendor: AcceleratorVendor::Nvidia as i32,
                            device_id: format!("GPU-{index}"),
                            vendor_index: u32::try_from(index).ok(),
                            memory_architecture: *architecture as i32,
                            total_memory_bytes: *total,
                            driver_or_runtime_version: "580.173.02".to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                },
            )),
        }],
        host: Some(HostObservation {
            result: Some(match host_total {
                Some(total_memory_bytes) => host_observation::Result::Facts(HostFacts {
                    total_memory_bytes,
                    ..Default::default()
                }),
                None => host_observation::Result::Unavailable(ObservationUnavailable::default()),
            }),
        }),
    }
}

/// The machine `spec` describes: named GPUs joined by `+` on the simulated host, or one named
/// machine.
fn described(spec: &str) -> MachineDescription {
    let choices: Vec<_> = spec.split('+').map(str::to_owned).collect();
    MachineDescription::simulated(&choices, "amd64").expect("simulated machine")
}
