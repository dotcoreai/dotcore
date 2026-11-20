//! Judges a memory estimate against one target's pools: the demand it puts on each pool, whether
//! that demand fits, the largest total context that fits and the context values a deployment starts
//! from, the largest value of a memory parameter that fits, and the preview a view draws of it.
//!
//! Dedicated devices hold the accelerator estimate in their own memory, while unified devices
//! take it from the host pool along with the host estimate. A declared minimum above a pool's
//! total exceeds it whatever the estimate, since it describes the hardware. A missing estimate or
//! pool fact reads as unknown rather than a guess.

use dotcore_catalog::shared_parameters::ContextRule;
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

impl Estimate {
    /// Bytes of one of the estimate's figures, absent when sizing gives none.
    fn bytes_of(&self, part: EstimatePart) -> Option<u64> {
        match part {
            EstimatePart::Accelerator => self.accelerator_bytes,
            EstimatePart::Host => self.host_bytes,
        }
    }
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

/// Returns the largest total context from the `lowest` total up to `highest`, when there is one,
/// whose demand fits every pool under the policy, given the demands at the `lowest` total and at
/// a `next` larger one, each paired with its total. Every sizing equation is linear in the total
/// context, so the two demands give each pool's slope. Absent when a pool fact is unknown, a
/// declared minimum is above a pool's total so no total context fits, the demand at the lowest
/// total doesn't fit, or a demand shrinks as the total grows.
pub(crate) fn largest_total_context(
    target: &TargetCapacity,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
    highest: Option<u64>,
    (lowest, at_lowest): (u64, &Demand),
    (next, at_next): (u64, &Demand),
) -> Option<LargestTotalContext> {
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
        // A zero growth leaves the bound to the other pools.
        let growth = at_next.bytes[index].checked_sub(first)?;
        if let Some(steps) = steps_within(room, next.checked_sub(lowest)?, growth) {
            let bound = lowest.saturating_add(steps);
            largest = Some(largest.map_or(bound, |largest| largest.min(bound)));
        }
    }
    Some(largest.map_or(LargestTotalContext::Unbounded, LargestTotalContext::Bounded))
}

/// Returns how many tokens past a sample a value growing by `growth` every `span` tokens may grow
/// before it uses more than `room`, saturating at the largest integer, or none when it doesn't grow.
pub(crate) fn steps_within(room: u64, span: u64, growth: u64) -> Option<u64> {
    let steps = (u128::from(room) * u128::from(span)).checked_div(u128::from(growth))?;
    Some(u64::try_from(steps).unwrap_or(u64::MAX))
}

/// How far the total context may grow on a target under a policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LargestTotalContext {
    /// The largest total context in tokens that fits.
    Bounded(u64),

    /// Nothing bounds it: no pool's demand grows with the total context and no ceiling applies.
    Unbounded,
}

/// Returns the largest value of a memory parameter at which the configuration still fits every
/// pool of the target under the policy, given `probe`, the estimate with the parameter at a value
/// (none when it can't be evaluated there, such as on overflow, which counts as not fitting).
///
/// Past the plateau, the smallest value beyond which neither estimate changes, more of the value
/// changes nothing, so the fit stops there, though never below the value sizing picks. The
/// plateau is probed first, since demand may step down there; below it, demand grows with the
/// value. The fit stays within the parameter's range and is its minimum when nothing fits. Absent
/// when the target has no pools, a pool fact is unknown, the estimate can't be evaluated at the
/// minimum, or it never changes with the value.
pub(crate) fn largest_fitting_value(
    target: &TargetCapacity,
    minimums: Option<&HardwareRequirements>,
    policy: Policy,
    range: MemoryParameterRange,
    mut probe: impl FnMut(u64) -> Option<Estimate>,
) -> Option<u64> {
    if target.pools.is_empty() {
        return None;
    }
    let room = target
        .pools
        .iter()
        .map(|pool| policy.usable_bytes(pool))
        .sum::<Option<u64>>()?;
    let ceiling = range.maximum.unwrap_or(u64::MAX).max(range.minimum);
    let mut sample = |value: u64| {
        let estimate = probe(value);
        let fits = estimate
            .and_then(|estimate| demand(&estimate, target))
            .is_some_and(|demand| fit(Some(&demand), target, minimums, policy) == MemoryFit::Fits);
        Sample {
            value,
            estimate,
            fits,
        }
    };

    let first = sample(range.minimum);
    first.estimate?;
    if ceiling == range.minimum {
        return None;
    }
    let mut samples = vec![first];
    let mut step = 1_u64;
    let growth = loop {
        let last = samples[samples.len() - 1];
        if last.value == ceiling {
            break Growth::Limit {
                edge: samples.len() - 1,
                top: ceiling,
            };
        }
        let next = sample(last.value.saturating_add(step).min(ceiling));
        samples.push(next);
        let Some(estimate) = next.estimate else {
            break Growth::Limit {
                edge: samples.len() - 2,
                top: next.value,
            };
        };
        if next.estimate == last.estimate {
            if samples.len() == 2 {
                return None;
            }
            // The estimate last changed between the two samples before this one.
            break Growth::Plateau {
                changed: samples[samples.len() - 3].value,
                flat: last,
            };
        }
        // The declared estimate never shrinks as the value grows, so once it alone takes more
        // than every pool's room, no larger value fits.
        if estimate
            .bytes_of(range.declared)
            .is_some_and(|bytes| bytes > room)
        {
            break Growth::Beyond(next.value);
        }
        step = step.saturating_mul(2);
    };

    // At a limit, the estimate may have stopped changing within the last step: the value just
    // below the last evaluated one tells.
    let plateau = match growth {
        Growth::Plateau { changed, flat } => Some((changed, flat)),
        Growth::Limit { edge, .. } => {
            let flat = samples[edge];
            let changed = edge.checked_sub(1).map(|before| samples[before].value)?;
            if flat.value - changed > 1 {
                let below = sample(flat.value - 1);
                samples.push(below);
                (below.estimate == flat.estimate).then_some((changed, flat))
            } else {
                None
            }
        }
        Growth::Beyond(_) => None,
    };
    let top = match (plateau, growth) {
        (Some((mut changed, flat)), _) => {
            let mut plateau = flat.value;
            while plateau - changed > 1 {
                let middle = sample(changed + (plateau - changed) / 2);
                samples.push(middle);
                if middle.estimate == flat.estimate {
                    plateau = middle.value;
                } else {
                    changed = middle.value;
                }
            }
            let sized = range.sized.unwrap_or(range.minimum);
            plateau.max(sized).min(ceiling)
        }
        (None, Growth::Limit { top, .. } | Growth::Beyond(top)) => top,
        (None, Growth::Plateau { flat, .. }) => flat.value,
    };

    let at_top = match samples.iter().find(|sample| sample.value == top) {
        Some(at_top) => *at_top,
        None => {
            let at_top = sample(top);
            samples.push(at_top);
            at_top
        }
    };
    if at_top.fits {
        return Some(top);
    }
    samples.retain(|sample| sample.value <= top);
    samples.sort_by_key(|sample| sample.value);
    // The top is one of them, so one exceeds.
    let exceeds = samples.iter().position(|sample| !sample.fits)?;
    let Some(fitting) = exceeds.checked_sub(1) else {
        return Some(range.minimum);
    };
    let (mut fitting, mut exceeding) = (samples[fitting].value, samples[exceeds].value);
    while exceeding - fitting > 1 {
        let middle = sample(fitting + (exceeding - fitting) / 2);
        if middle.fits {
            fitting = middle.value;
        } else {
            exceeding = middle.value;
        }
    }
    Some(fitting)
}

/// The values a memory parameter may take, the one sizing picks, and the estimate it feeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MemoryParameterRange {
    /// Inclusive minimum value.
    pub(crate) minimum: u64,

    /// Inclusive maximum value, none when unbounded.
    pub(crate) maximum: Option<u64>,

    /// Value sizing picks when the parameter is left empty, none when it picks none.
    pub(crate) sized: Option<u64>,

    /// Estimate the value feeds, which never shrinks as the value grows where it is available.
    pub(crate) declared: EstimatePart,
}

/// One of the two memory figures of an estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EstimatePart {
    Accelerator,
    Host,
}

/// A probed value with its estimate and whether that fits.
#[derive(Clone, Copy, Debug)]
struct Sample {
    value: u64,
    estimate: Option<Estimate>,
    fits: bool,
}

/// Where the growing search of a memory parameter stopped.
#[derive(Clone, Copy, Debug)]
enum Growth {
    /// The estimate stopped changing: it last changed past `changed` and holds from `flat` on.
    Plateau { changed: u64, flat: Sample },

    /// The maximum was reached at `top`, or `top` couldn't be evaluated; the sample at `edge` is
    /// the last one evaluated.
    Limit { edge: usize, top: u64 },

    /// The declared estimate alone takes more than every pool's room from this value on.
    Beyond(u64),
}

/// Returns the context values a deployment starts from: the requested context per request and the
/// requested total context, or else the one the engine's `rule` derives for them, capped by the
/// largest total context that fits. When even one request at the requested size can't fit, the context per
/// request drops to what the rule fills from the largest total. Without a largest fit, the
/// requested values stand.
pub(crate) fn starting_context(
    requested: StartingContextRequest,
    largest_total_context: Option<u64>,
    rule: &dyn ContextRule,
) -> StartingContext {
    let StartingContextRequest {
        context_tokens_per_request,
        parallel_requests,
        total_context_tokens,
        maximum_context_tokens,
    } = requested;
    let starting_per_request =
        largest_total_context.map_or(context_tokens_per_request, |largest| {
            context_tokens_per_request.min(rule.filled_context_per_request(
                largest,
                parallel_requests,
                maximum_context_tokens,
            ))
        });
    // A requested total stays as asked, within what fits.
    let asked = match total_context_tokens {
        Some(total_context) => StartingTotalContext::Requested(total_context),
        None => StartingTotalContext::PerRequestTimesParallel(rule.derived_total_context(
            starting_per_request,
            parallel_requests,
            maximum_context_tokens,
        )),
    };
    let total_context = match largest_total_context {
        Some(largest) if largest < asked.tokens() => StartingTotalContext::LargestFit(largest),
        _ => asked,
    };
    StartingContext {
        context_tokens_per_request: starting_per_request,
        lowered: starting_per_request < context_tokens_per_request,
        total_context,
    }
}

/// The context values a starting deployment is asked for, before the target caps them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartingContextRequest {
    pub(crate) context_tokens_per_request: u64,
    pub(crate) parallel_requests: u64,

    /// Total context asked for; none follows the context per request by the engine's rule.
    pub(crate) total_context_tokens: Option<u64>,

    /// Largest context one request may use on the configuration.
    pub(crate) maximum_context_tokens: u64,
}

/// The context values a deployment starts from on one target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StartingContext {
    pub(crate) context_tokens_per_request: u64,

    /// Whether the context per request is below the requested one, since one request at that
    /// size doesn't fit.
    pub(crate) lowered: bool,

    pub(crate) total_context: StartingTotalContext,
}

/// The total context in tokens a deployment starts from, by where it comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartingTotalContext {
    /// The largest total context that fits, below what the context per request derives.
    LargestFit(u64),

    /// What the engine's rule derives from the context per request and the parallel requests.
    PerRequestTimesParallel(u64),

    /// The total context asked for.
    Requested(u64),
}

impl StartingTotalContext {
    /// The total context in tokens, whatever its source.
    pub(crate) fn tokens(self) -> u64 {
        match self {
            Self::LargestFit(tokens)
            | Self::PerRequestTimesParallel(tokens)
            | Self::Requested(tokens) => tokens,
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
