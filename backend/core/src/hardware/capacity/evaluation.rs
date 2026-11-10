//! Judges a memory estimate against one target's pools: the demand it puts on each pool, whether
//! that demand fits, the largest total context that fits and the one a deployment starts from, and
//! the preview a view draws of it.
//!
//! Dedicated devices hold the accelerator estimate in their own memory, while unified devices
//! take it from the host pool along with the host estimate. A declared minimum above a pool's
//! total exceeds it whatever the estimate, since it describes the hardware. A missing estimate or
//! pool fact reads as unknown rather than a guess.

use dotcore_catalog::shared_parameters::MINIMUM_CONTEXT_TOKENS;
use dotcore_proto::dotcore::{catalog::v1::HardwareRequirements, deployment::v1::MemoryFit};

use super::{Pool, PoolKind, PoolView, TargetCapacity};

/// Share of a device pool's total, in percent, left out of its free memory before anything is
/// judged to fit there, so normal variation in use doesn't tip a pool filled to the line over
/// capacity.
const DEVICE_HEADROOM_PERCENT: u64 = 1;

/// Bytes of a host pool left out of its free memory before anything is judged to fit there. The
/// operating system, the node's other processes and each engine's own host processes live in host
/// memory and draw from it as a workload starts, whatever the pool's size, so the reserve is a
/// fixed amount rather than a share of the total. On unified devices, accelerator memory comes
/// from this pool too and can't be paged out, so running short stalls the whole host.
const HOST_RESERVE_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Largest share of a host pool's total the reserve takes, as a divisor, so a small host keeps
/// most of its memory for workloads.
const HOST_RESERVE_MAXIMUM_DIVISOR: u64 = 8;

/// What a demand is compared with in each pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Policy {
    basis: Basis,
    headroom: bool,
}

impl Policy {
    /// Compares with what is free now, less a headroom: does it fit beside what runs?
    pub(crate) const FREE_NOW: Self = Self {
        basis: Basis::Free,
        headroom: true,
    };

    /// Compares with the whole total: can this hardware run it at all?
    pub(crate) const TOTAL: Self = Self {
        basis: Basis::Total,
        headroom: false,
    };

    /// Bytes a demand may take from the pool, absent when a fact it needs is unknown.
    pub(super) fn usable_bytes(self, pool: &Pool) -> Option<u64> {
        let total = pool.total_bytes?;
        let basis = match self.basis {
            Basis::Free => pool.free_bytes()?,
            Basis::Total => total,
        };
        Some(basis.saturating_sub(self.headroom_bytes(pool.kind, total)))
    }

    /// The pool's use at or below which a demand fits: its total less the headroom, absent when
    /// the total is unknown.
    fn fit_line_bytes(self, pool: &Pool) -> Option<u64> {
        pool.total_bytes
            .map(|total| total.saturating_sub(self.headroom_bytes(pool.kind, total)))
    }

    /// Bytes of a pool's `total` left out before anything is judged to fit: a share of a device
    /// pool, the reserve of a host pool.
    fn headroom_bytes(self, kind: PoolKind, total: u64) -> u64 {
        if !self.headroom {
            return 0;
        }
        match kind {
            PoolKind::Device => total * DEVICE_HEADROOM_PERCENT / 100,
            PoolKind::Host => host_reserve_bytes(total),
        }
    }
}

/// Bytes a host pool of `total` bytes keeps free for the operating system and a starting
/// workload's processes: the fixed reserve, at most an eighth of the total.
pub(crate) fn host_reserve_bytes(total: u64) -> u64 {
    HOST_RESERVE_BYTES.min(total / HOST_RESERVE_MAXIMUM_DIVISOR)
}

/// The pool measure a policy compares with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Basis {
    Free,
    Total,
}

/// A memory estimate for one configuration, as sizing produces it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Estimate {
    /// Accelerator memory, absent when the configuration isn't sized.
    pub(crate) accelerator_bytes: Option<u64>,

    /// Host memory outside the accelerator; absent when sizing declares no host footprint.
    pub(crate) host_bytes: Option<u64>,
}

/// Bytes an estimate needs from each pool of one target, in the target's pool order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Demand {
    bytes: Vec<u64>,
}

impl Demand {
    /// Bytes needed from each pool, in the target's pool order.
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u64] {
        &self.bytes
    }
}

/// Turns an estimate into a demand on the target's pools: the accelerator estimate on its
/// accelerator pools, split evenly across several devices with the first taking any remainder,
/// and the host estimate on its host pool. Unified devices put both on the host pool. Absent
/// without an accelerator estimate.
pub(crate) fn demand(estimate: &Estimate, target: &TargetCapacity) -> Option<Demand> {
    let accelerator_bytes = estimate.accelerator_bytes?;
    let host_bytes = estimate.host_bytes.unwrap_or_default();
    let accelerator_kind = target.accelerator_pool_kind();
    let accelerator_pools = target.accelerator_pools().count();
    let mut accelerator_index = 0;
    let bytes = target
        .pools
        .iter()
        .map(|pool| {
            let accelerator = if pool.kind == accelerator_kind {
                accelerator_index += 1;
                device_share(accelerator_bytes, accelerator_pools, accelerator_index - 1)
            } else {
                0
            };
            match pool.kind {
                PoolKind::Device => accelerator,
                PoolKind::Host => accelerator.saturating_add(host_bytes),
            }
        })
        .collect();
    Some(Demand { bytes })
}

/// Judges a demand and the configuration's declared minimums against every pool of the target.
/// A minimum above a pool's total exceeds it even without a demand; otherwise the demand fits a
/// pool when it is at most what the policy lets it use. Any pool exceeded exceeds, any pool
/// unknown leaves the answer unknown, and a target with no pools is unknown.
pub(crate) fn fit(
    demand: Option<&Demand>,
    target: &TargetCapacity,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
) -> MemoryFit {
    if target.pools.is_empty() {
        return MemoryFit::Unknown;
    }
    let accelerator_kind = target.accelerator_pool_kind();
    let verdicts = target.pools.iter().enumerate().map(|(index, pool)| {
        pool_fit(
            pool,
            pool.kind == accelerator_kind,
            demand.map(|demand| demand.bytes[index]),
            minimums,
            policy,
        )
    });
    verdicts.fold(MemoryFit::Fits, |combined, verdict| {
        match (combined, verdict) {
            (MemoryFit::Exceeds, _) | (_, MemoryFit::Exceeds) => MemoryFit::Exceeds,
            (MemoryFit::Unknown, _) | (_, MemoryFit::Unknown) => MemoryFit::Unknown,
            _ => MemoryFit::Fits,
        }
    })
}

/// Returns the largest total context in `lowest..=highest` whose demand fits every pool under the
/// policy, given the demands at `lowest` and at the next total. Every sizing equation is linear in
/// the total context, so the two demands give each pool's slope. Absent when a pool fact is
/// unknown, a declared minimum is above a pool's total so no total context fits, the demand at
/// `lowest` doesn't fit, or a demand shrinks as the total grows.
pub(crate) fn largest_total_context(
    target: &TargetCapacity,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
    lowest: u64,
    highest: u64,
    at_lowest: &Demand,
    at_next: &Demand,
) -> Option<u64> {
    if target.pools.is_empty() {
        return None;
    }
    let accelerator_kind = target.accelerator_pool_kind();
    let below_minimum = target.pools.iter().any(|pool| {
        pool.total_bytes.is_some_and(|total| {
            minimum_exceeds(pool, pool.kind == accelerator_kind, total, minimums)
        })
    });
    if below_minimum {
        return None;
    }
    let mut largest = highest;
    for (index, pool) in target.pools.iter().enumerate() {
        let first = at_lowest.bytes[index];
        let room = policy.usable_bytes(pool)?.checked_sub(first)?;
        // A zero slope leaves the bound to the other pools.
        let slope = at_next.bytes[index].checked_sub(first)?;
        if let Some(steps) = room.checked_div(slope) {
            largest = largest.min(lowest + steps);
        }
    }
    Some(largest)
}

/// Returns the total context a deployment starts from: the largest total context that fits,
/// rounded down to whole blocks of the smallest context a deployment accepts, or the profile's
/// `default` when there is no largest fit of at least one block.
pub(crate) fn starting_total_context(
    largest: Option<u64>,
    default: Option<u64>,
) -> Option<StartingTotalContext> {
    largest
        .map(|tokens| tokens / MINIMUM_CONTEXT_TOKENS * MINIMUM_CONTEXT_TOKENS)
        .filter(|tokens| *tokens >= MINIMUM_CONTEXT_TOKENS)
        .map(StartingTotalContext::LargestFit)
        .or(default.map(StartingTotalContext::ProfileDefault))
}

/// The total context in tokens a deployment starts from, by where it comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartingTotalContext {
    /// The largest total context that fits, in whole blocks.
    LargestFit(u64),

    /// The profile's default, without a largest fit of at least one block.
    ProfileDefault(u64),
}

impl StartingTotalContext {
    /// The total context in tokens, whatever its source.
    pub(crate) fn tokens(self) -> u64 {
        match self {
            Self::LargestFit(tokens) | Self::ProfileDefault(tokens) => tokens,
        }
    }
}

/// What a view draws for one configuration on one target: each pool with what occupies it, the
/// configuration's demand on top, and the free-memory fit line it is judged against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Preview {
    /// The target's pools, in its pool order.
    pub(crate) pools: Vec<PoolPreview>,

    /// Fit of the demand on every pool, as [`fit`] judges it on free memory.
    pub(crate) free_memory_fit: MemoryFit,
}

/// One pool of a preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolPreview {
    /// What the pool draws with the demand on top.
    pub(crate) pool: PoolView,

    /// Drawn use at or below this line fits; absent when the total is unknown.
    pub(crate) fit_line_bytes: Option<u64>,

    /// Fit of the demand on this pool alone.
    pub(crate) free_memory_fit: MemoryFit,

    /// Drawn use past the total when it is beyond it, otherwise past the fit line; zero when the
    /// demand puts nothing on the pool, fits, or a fact is unknown.
    pub(crate) over_by_bytes: u64,

    /// Whether the demand takes drawn use beyond the total, not only past the fit line.
    pub(crate) beyond_total: bool,
}

/// Previews a demand on the target's pools, with its occupancy, judged on free memory: the same
/// verdicts [`fit`] gives, with the numbers that explain them.
pub(crate) fn preview(
    demand: Option<&Demand>,
    target: &TargetCapacity,
    minimums: Option<&HardwareRequirements>,
) -> Preview {
    let policy = Policy::FREE_NOW;
    let accelerator_kind = target.accelerator_pool_kind();
    let pools = target
        .pools
        .iter()
        .enumerate()
        .map(|(index, pool)| {
            let demand_bytes = demand.map(|demand| demand.bytes[index]);
            let view = pool.view(demand_bytes);
            let fit_line_bytes = policy.fit_line_bytes(pool);
            let drawn_bytes = demand_bytes.and_then(|demand| pool.drawn_bytes(demand));
            let (over_by_bytes, beyond_total) = match (demand_bytes, drawn_bytes, fit_line_bytes) {
                (Some(demand), Some(drawn), Some(line)) if demand > 0 && drawn > line => {
                    if view.over_total_bytes > 0 {
                        (view.over_total_bytes, true)
                    } else {
                        (drawn - line, false)
                    }
                }
                _ => (0, false),
            };
            PoolPreview {
                pool: view,
                fit_line_bytes,
                free_memory_fit: pool_fit(
                    pool,
                    pool.kind == accelerator_kind,
                    demand_bytes,
                    minimums,
                    policy,
                ),
                over_by_bytes,
                beyond_total,
            }
        })
        .collect();
    Preview {
        pools,
        free_memory_fit: fit(demand, target, minimums, policy),
    }
}

/// The share of `bytes` the device at `index` of `devices` takes when they are split evenly, the
/// first device taking any remainder.
pub(super) fn device_share(bytes: u64, devices: usize, index: usize) -> u64 {
    let devices = devices.max(1) as u64;
    let share = bytes / devices;
    if index == 0 {
        share + bytes % devices
    } else {
        share
    }
}

/// Whether a per-device memory minimum is above a pool's total, when that pool is the device's or
/// stands in for it; an unknown total or no minimum never is.
pub(crate) fn below_device_minimum(
    pool_total_bytes: Option<u64>,
    minimums: Option<&HardwareRequirements>,
) -> bool {
    pool_total_bytes.is_some_and(|total| {
        minimums.is_some_and(|minimums| minimums.minimum_memory_bytes_per_device > total)
    })
}

/// Combines a configuration's declared minimums with extra ones a caller adds on top, such as a
/// model recommendation's, into the larger of each: the result is above a pool's total whenever
/// either one is. Absent when neither declares any; a zero value sets no minimum.
pub(crate) fn combined_minimums(
    declared: Option<&HardwareRequirements>,
    extra: Option<&HardwareRequirements>,
) -> Option<HardwareRequirements> {
    match (declared, extra) {
        (None, None) => None,
        (Some(minimums), None) | (None, Some(minimums)) => Some(*minimums),
        (Some(declared), Some(extra)) => Some(HardwareRequirements {
            minimum_system_memory_bytes: declared
                .minimum_system_memory_bytes
                .max(extra.minimum_system_memory_bytes),
            minimum_memory_bytes_per_device: declared
                .minimum_memory_bytes_per_device
                .max(extra.minimum_memory_bytes_per_device),
        }),
    }
}

/// Judges a pool's share of a demand and the declared minimums it can be compared with: a minimum
/// above its total exceeds it, otherwise the share fits when it is at most what the policy lets it
/// use, and a missing share or pool fact is unknown.
fn pool_fit(
    pool: &Pool,
    accelerator_pool: bool,
    demand_bytes: Option<u64>,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
) -> MemoryFit {
    let Some(total) = pool.total_bytes else {
        return MemoryFit::Unknown;
    };
    if minimum_exceeds(pool, accelerator_pool, total, minimums) {
        return MemoryFit::Exceeds;
    }
    match (demand_bytes, policy.usable_bytes(pool)) {
        (Some(demand), Some(usable)) if demand <= usable => MemoryFit::Fits,
        (Some(_), Some(_)) => MemoryFit::Exceeds,
        _ => MemoryFit::Unknown,
    }
}

/// Whether a declared minimum the pool can be compared with is above its total: the per-device
/// minimum on each accelerator pool, and the system memory minimum on the host pool.
fn minimum_exceeds(
    pool: &Pool,
    accelerator_pool: bool,
    total: u64,
    minimums: Option<&HardwareRequirements>,
) -> bool {
    let device = accelerator_pool && below_device_minimum(Some(total), minimums);
    let system = pool.kind == PoolKind::Host
        && minimums.is_some_and(|minimums| minimums.minimum_system_memory_bytes > total);
    device || system
}
