//! The capacity component: memory pools, their occupancy, and the evaluation of a memory estimate
//! against them, as plain data and pure functions.
//!
//! It builds the pools an execution target draws from, and every pool of a node, out of a node's
//! reconciled memory facts and what each workload holds there, turns an estimate into a demand on those pools, and judges
//! whether the demand fits, how large a total context fits, and what a view draws for it. Every
//! answer is advice: nothing here reserves, admits, or places a workload, and unknown or stale
//! facts give an unknown answer rather than a guess.

mod evaluation;
mod node;

use std::collections::BTreeMap;

use dotcore_domain::MemoryArchitecture;
use dotcore_site::node::target_inventory::{CapabilityObservation, NodeMemoryFacts};

use evaluation::device_share;
pub(crate) use evaluation::{
    Estimate, Policy, PoolPreview, StartingTotalContext, below_device_minimum, demand, fit,
    largest_total_context, preview, starting_total_context,
};
pub(crate) use node::NodeCapacity;

/// What the workloads of each site node hold of its memory, by node.
pub(crate) type NodeWorkloads = BTreeMap<String, Vec<Workload>>;

/// The memory pools one execution target draws from, with what is in use in each.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TargetCapacity {
    pools: Vec<Pool>,
}

impl TargetCapacity {
    /// Builds the pools a target of a node with `memory_architecture` draws from. A dedicated
    /// target draws from one pool per device it holds, `device_ids` naming them, and from its
    /// node's host pool; when its devices aren't known, one device pool of unknown size stands for
    /// them, so its answers read unknown. A unified target draws from its node's host pool only,
    /// which its devices share.
    pub(crate) fn for_target(
        memory_architecture: MemoryArchitecture,
        node: &NodeHardware,
        device_ids: Option<&[String]>,
    ) -> Self {
        let pools = match memory_architecture {
            MemoryArchitecture::Dedicated => {
                let devices = device_ids
                    .filter(|ids| !ids.is_empty())
                    .and_then(|ids| ids.iter().map(|id| node.device_pool(id)).collect());
                let mut pools = devices.unwrap_or_else(|| vec![Pool::unknown_device()]);
                pools.push(node.host_pool());
                pools
            }
            MemoryArchitecture::Unified => vec![node.host_pool()],
        };
        Self { pools }
    }

    /// Counts in the target's pools what the workloads of its node hold there, each in the pools
    /// it shares with the target: its own devices and the node's host pool. Starting workloads
    /// claim memory beyond what is measured of them, and a claim whose size or devices aren't known
    /// makes the pools it may use unknown. Other workloads only split the measured use: what the
    /// node attributes to them, or a ready workload's estimate where nothing is attributed.
    pub(crate) fn with_workloads(mut self, workloads: &[Workload]) -> Self {
        occupy(&mut self.pools, workloads);
        self
    }

    /// The pools the target's accelerator memory comes from: its device pools, or the host pool
    /// when its devices share host memory.
    pub(crate) fn accelerator_pools(&self) -> impl Iterator<Item = &Pool> {
        let accelerator = self.accelerator_pool_kind();
        self.pools
            .iter()
            .filter(move |pool| pool.kind == accelerator)
    }

    /// Total of the smallest pool the target's accelerator memory comes from, absent when any of
    /// their totals is unknown.
    pub(crate) fn accelerator_total_bytes(&self) -> Option<u64> {
        self.accelerator_pools()
            .map(|pool| pool.total_bytes)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .min()
    }

    /// The kind of pool accelerator memory comes from: devices when the target has any, the host
    /// otherwise.
    fn accelerator_pool_kind(&self) -> PoolKind {
        if self.pools.iter().any(|pool| pool.kind == PoolKind::Device) {
            PoolKind::Device
        } else {
            PoolKind::Host
        }
    }
}

/// Memory one or more workloads draw from: an accelerator device's own memory, or a node's host
/// memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pool {
    pub(crate) kind: PoolKind,

    /// The device whose memory a device pool is, absent for the host pool or an unknown device.
    pub(crate) device: Option<PoolDevice>,

    /// Bytes the pool holds, absent when they can't be determined.
    pub(crate) total_bytes: Option<u64>,

    pub(crate) occupancy: Occupancy,
}

impl Pool {
    /// Bytes neither in use nor claimed, absent when the total, the measured use or a claim is
    /// unknown.
    pub(crate) fn free_bytes(&self) -> Option<u64> {
        Some(self.total_bytes?.saturating_sub(self.drawn_bytes(0)?))
    }

    /// Bytes in use and claimed, with `demand_bytes` added: the use a view draws. Absent when the
    /// measured use or a claim is unknown.
    fn drawn_bytes(&self, demand_bytes: u64) -> Option<u64> {
        Some(
            self.occupancy
                .measured_bytes?
                .saturating_add(self.occupancy.claimed_bytes?)
                .saturating_add(demand_bytes),
        )
    }

    /// Drawn use with `demand_bytes` added past the pool's total; zero when it stays within the
    /// total or a fact is unknown.
    fn over_total_bytes(&self, demand_bytes: u64) -> u64 {
        self.total_bytes
            .zip(self.drawn_bytes(demand_bytes))
            .map_or(0, |(total, drawn)| drawn.saturating_sub(total))
    }

    /// What a view draws of the pool, with `demand_bytes` of a previewed configuration on top.
    fn view(&self, demand_bytes: Option<u64>) -> PoolView {
        let other = self.occupancy.other_bytes().map(|bytes| Segment {
            source: SegmentSource::Other,
            workload: None,
            bytes,
        });
        let previewed = demand_bytes.map(|bytes| Segment {
            source: SegmentSource::Previewed,
            workload: None,
            bytes,
        });
        PoolView {
            kind: self.kind,
            device: self.device.clone(),
            total_bytes: self.total_bytes,
            segments: self
                .occupancy
                .workloads
                .iter()
                .cloned()
                .chain(other)
                .chain(previewed)
                .filter(|segment| segment.bytes > 0)
                .collect(),
            over_total_bytes: self.over_total_bytes(demand_bytes.unwrap_or_default()),
        }
    }

    /// The id of the device whose memory a device pool is.
    fn device_id(&self) -> Option<&str> {
        self.device.as_ref().map(|device| device.id.as_str())
    }

    /// A device pool whose size and use are unknown.
    fn unknown_device() -> Self {
        Self {
            kind: PoolKind::Device,
            device: None,
            total_bytes: None,
            occupancy: Occupancy::default(),
        }
    }
}

/// What a view draws of one pool: what occupies it, segment by segment, and how far that use goes
/// past its total.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolView {
    pub(crate) kind: PoolKind,

    /// The device whose memory a device pool is, absent for the host pool or an unknown device.
    pub(crate) device: Option<PoolDevice>,

    pub(crate) total_bytes: Option<u64>,

    /// Each workload's bytes in workload order, then other use, then a previewed demand: together
    /// the drawn use. Empty segments are left out.
    pub(crate) segments: Vec<Segment>,

    /// Drawn use past the total; zero when it stays within the total or a fact is unknown.
    pub(crate) over_total_bytes: u64,
}

/// The accelerator device a device pool is the memory of, as its node reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct PoolDevice {
    /// Vendor-native identity of the device.
    pub(crate) id: String,

    /// Product name the vendor reports; empty when unreported.
    pub(crate) model: String,

    /// Enumeration index the vendor gives the device, absent when unreported.
    pub(crate) vendor_index: Option<u32>,
}

/// Where a pool's memory lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PoolKind {
    /// Dedicated memory of one accelerator device.
    Device,

    /// A node's host memory, which unified devices and host-side estimates share.
    Host,
}

/// What is in use in one memory pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occupancy {
    /// Bytes in use in the latest fresh observation, absent when it is unknown or stale. Transient:
    /// it moves with every observation and is never retained.
    pub(crate) measured_bytes: Option<u64>,

    /// Bytes starting workloads are expected to take beyond what is measured of them: their
    /// claims. Absent when a starting workload that may use the pool has an unknown claim there.
    /// Advisory, never a reservation.
    pub(crate) claimed_bytes: Option<u64>,

    /// What each workload holds of the measured and claimed bytes, in workload order; the rest of
    /// the measured use is other use.
    pub(crate) workloads: Vec<Segment>,
}

impl Occupancy {
    /// Measured use no workload accounts for, absent when the measured use is unknown.
    pub(crate) fn other_bytes(&self) -> Option<u64> {
        let accounted = self
            .workloads
            .iter()
            .filter(|segment| segment.source != SegmentSource::Claimed)
            .map(|segment| segment.bytes)
            .sum::<u64>();
        Some(self.measured_bytes?.saturating_sub(accounted))
    }
}

impl Default for Occupancy {
    /// Nothing measured and nothing claimed.
    fn default() -> Self {
        Self {
            measured_bytes: None,
            claimed_bytes: Some(0),
            workloads: Vec::new(),
        }
    }
}

/// One part of a pool's use: bytes one workload holds, other use, or a previewed demand.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) source: SegmentSource,

    /// The workload the bytes belong to, absent for other use and a previewed demand.
    pub(crate) workload: Option<WorkloadKey>,

    pub(crate) bytes: u64,
}

/// What a segment's bytes stand for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SegmentSource {
    /// Use the node measures and attributes to the workload.
    Measured,

    /// A ready workload's estimate standing for measured use the node doesn't attribute to it.
    Estimated,

    /// A starting workload's estimate beyond what is measured of it.
    Claimed,

    /// Measured use no workload accounts for.
    Other,

    /// The demand of the configuration a preview judges.
    Previewed,
}

/// A workload on a node and the memory it holds there: its estimate on the pools its own target
/// draws from, and the accelerator memory the node attributes to it. While it starts, it claims its
/// estimate: its accelerator estimate counts beyond what the node already attributes to it, while
/// its host estimate counts whole, since attribution sees only accelerator memory and host memory
/// loads through the whole start. Once ready and measured, its claim ends and it holds only
/// measured use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Workload {
    /// Identity of the workload, which a pool's segments name.
    pub(crate) key: WorkloadKey,

    pub(crate) phase: WorkloadPhase,

    /// Whether the workload is expected to hold memory in the pools it draws from, so a node view
    /// lists it as unsized in such a pool where it holds nothing.
    pub(crate) expects_use: bool,

    /// Memory architecture of the workload's target.
    pub(crate) memory_architecture: MemoryArchitecture,

    /// Vendor-native ids of the devices the workload's target holds; empty when they don't
    /// resolve.
    pub(crate) device_ids: Vec<String>,

    pub(crate) estimate: Estimate,

    /// Accelerator memory the node attributes to the workload's processes, by device id; absent
    /// until the node attributes any.
    pub(crate) attributed: Option<BTreeMap<String, u64>>,
}

/// Identity of a workload across the lanes that run workloads, so two lanes can't name the same
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkloadKey {
    pub(crate) lane: WorkloadLane,

    /// Identity of the workload within its lane.
    pub(crate) id: String,
}

impl WorkloadKey {
    /// A workload of the inference lane.
    pub(crate) fn inference(id: impl Into<String>) -> Self {
        Self {
            lane: WorkloadLane::Inference,
            id: id.into(),
        }
    }

    /// A workload of the applications lane.
    pub(crate) fn applications(id: impl Into<String>) -> Self {
        Self {
            lane: WorkloadLane::Applications,
            id: id.into(),
        }
    }
}

/// The lane that runs a workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkloadLane {
    /// Model workloads serving inference.
    Inference,

    /// Application workloads.
    Applications,
}

/// Where a workload is in its life, which decides how it counts in a pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkloadPhase {
    /// Not ready yet, or ready before any reading measured it: it claims its estimate.
    Starting,

    /// Ready: it holds its attributed use, or its estimate where nothing is attributed.
    Ready,

    /// Any other phase, such as stopping: it holds only what is attributed to it.
    Other,
}

impl Workload {
    /// What the workload claims in `pool` while it starts, absent when it may use the pool but its
    /// estimate there or its devices aren't known. A dedicated workload whose devices don't resolve
    /// may be on any device of its node. A unified device's memory is the host's, and everything
    /// the node attributes to the workload is in it, whichever device id the attribution carries.
    fn claim_in(&self, pool: &Pool) -> Option<ClaimShare> {
        let host_bytes = self.estimate.host_bytes.unwrap_or_default();
        match (self.memory_architecture, pool.kind) {
            (MemoryArchitecture::Dedicated, PoolKind::Device) => {
                if self.device_ids.is_empty() {
                    return None;
                }
                let Some(index) = self.device_index(pool) else {
                    return Some(ClaimShare::NONE);
                };
                let accelerator_bytes = self.estimate.accelerator_bytes?;
                Some(ClaimShare {
                    loading_bytes: device_share(accelerator_bytes, self.device_ids.len(), index),
                    attributed_bytes: self.attributed_on(&self.device_ids[index]),
                    held_bytes: 0,
                })
            }
            (MemoryArchitecture::Dedicated, PoolKind::Host) => Some(ClaimShare {
                held_bytes: host_bytes,
                ..ClaimShare::NONE
            }),
            (MemoryArchitecture::Unified, PoolKind::Host) => Some(ClaimShare {
                loading_bytes: self.estimate.accelerator_bytes?,
                attributed_bytes: self.attributed_total(),
                held_bytes: host_bytes,
            }),
            (MemoryArchitecture::Unified, PoolKind::Device) => Some(ClaimShare::NONE),
        }
    }

    /// What a workload that isn't starting holds of `pool`'s measured use, before it is capped by
    /// that use: the memory attributed to it there, or, when ready with nothing attributed there,
    /// its estimate's share of the pool. Host memory is never attributed, so a dedicated
    /// workload's host estimate always stands for its host use.
    fn use_in(&self, pool: &Pool) -> (SegmentSource, u64) {
        let attributed = match (self.memory_architecture, pool.kind) {
            (MemoryArchitecture::Dedicated, PoolKind::Device) => pool
                .device_id()
                .map(|device_id| self.attributed_on(device_id))
                .unwrap_or_default(),
            (MemoryArchitecture::Unified, PoolKind::Host) => self.attributed_total(),
            _ => 0,
        };
        if attributed > 0 || self.phase != WorkloadPhase::Ready {
            return (SegmentSource::Measured, attributed);
        }
        let host_bytes = self.estimate.host_bytes.unwrap_or_default();
        let estimated = match (self.memory_architecture, pool.kind) {
            (MemoryArchitecture::Dedicated, PoolKind::Device) => {
                match (self.device_index(pool), self.estimate.accelerator_bytes) {
                    (Some(index), Some(bytes)) => device_share(bytes, self.device_ids.len(), index),
                    _ => 0,
                }
            }
            (MemoryArchitecture::Dedicated, PoolKind::Host) => host_bytes,
            (MemoryArchitecture::Unified, PoolKind::Host) => self
                .estimate
                .accelerator_bytes
                .map_or(0, |bytes| bytes.saturating_add(host_bytes)),
            (MemoryArchitecture::Unified, PoolKind::Device) => 0,
        };
        (SegmentSource::Estimated, estimated)
    }

    /// Whether the workload may use `pool`: a device it holds, or any device when its devices
    /// don't resolve; the host pool when its devices share host memory, or when it has a host
    /// estimate beside dedicated devices.
    fn draws_from(&self, pool: &Pool) -> bool {
        match (self.memory_architecture, pool.kind) {
            (MemoryArchitecture::Dedicated, PoolKind::Device) => {
                self.device_ids.is_empty() || self.device_index(pool).is_some()
            }
            (MemoryArchitecture::Dedicated, PoolKind::Host) => {
                self.estimate.host_bytes.is_some_and(|bytes| bytes > 0)
            }
            (MemoryArchitecture::Unified, PoolKind::Host) => true,
            (MemoryArchitecture::Unified, PoolKind::Device) => false,
        }
    }

    /// The workload's `bytes` of a pool, as a segment of `source`.
    fn segment(&self, source: SegmentSource, bytes: u64) -> Segment {
        Segment {
            source,
            workload: Some(self.key.clone()),
            bytes,
        }
    }

    /// Position of a device pool's device among the workload's devices, absent when it isn't one.
    fn device_index(&self, pool: &Pool) -> Option<usize> {
        let device_id = pool.device_id()?;
        self.device_ids.iter().position(|id| id == device_id)
    }

    /// Accelerator memory the node attributes to the workload on one device.
    fn attributed_on(&self, device_id: &str) -> u64 {
        self.attributed
            .as_ref()
            .and_then(|attributed| attributed.get(device_id).copied())
            .unwrap_or_default()
    }

    /// Accelerator memory the node attributes to the workload over all of its devices.
    fn attributed_total(&self) -> u64 {
        self.attributed
            .as_ref()
            .map(|attributed| attributed.values().sum())
            .unwrap_or_default()
    }
}

/// What one claim expects in one pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClaimShare {
    /// Estimate the pool's measured use takes over as the workload's attributed use grows.
    loading_bytes: u64,

    /// Use the node attributes to the workload in the pool.
    attributed_bytes: u64,

    /// Estimate counted whole until the workload is ready.
    held_bytes: u64,
}

impl ClaimShare {
    /// A claim on a pool the workload doesn't use.
    const NONE: Self = Self {
        loading_bytes: 0,
        attributed_bytes: 0,
        held_bytes: 0,
    };
}

/// One node's memory as its latest observation reports it: each accelerator device and the host,
/// from which each of its targets takes the pools it draws from. Measured use is kept only from a
/// fresh observation; totals hold while the observation ages.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeHardware {
    devices: Vec<DeviceMemory>,
    host: Option<HostMemory>,
}

impl NodeHardware {
    /// Reads a node's reconciled memory facts; a stale observation keeps its totals only.
    pub(crate) fn observed(memory: &CapabilityObservation<NodeMemoryFacts>) -> Self {
        match memory {
            CapabilityObservation::Available(facts) => Self::read(facts, true),
            CapabilityObservation::Stale(facts) => Self::read(facts, false),
            CapabilityObservation::Unavailable => Self::default(),
        }
    }

    /// Reads memory facts, keeping measured use only when `fresh`.
    fn read(facts: &NodeMemoryFacts, fresh: bool) -> Self {
        Self {
            devices: facts
                .devices
                .iter()
                .map(|device| DeviceMemory {
                    device: PoolDevice {
                        id: device.device_id.clone(),
                        model: device.model.clone(),
                        vendor_index: device.vendor_index,
                    },
                    total_bytes: device.total_bytes,
                    used_bytes: device.used_bytes.filter(|_| fresh),
                })
                .collect(),
            host: facts.host.map(|host| HostMemory {
                total_bytes: host.total_bytes,
                available_bytes: fresh.then_some(host.available_bytes),
            }),
        }
    }

    /// The pool of the device with `device_id`, absent when the node reported no such device. Its
    /// size or use is unknown when the device didn't report it.
    fn device_pool(&self, device_id: &str) -> Option<Pool> {
        self.devices
            .iter()
            .find(|device| device.device.id == device_id)
            .map(DeviceMemory::pool)
    }

    /// The host pool, in use by all but what the host reports available; unknown without a
    /// reported total.
    fn host_pool(&self) -> Pool {
        let host = self.host.filter(|host| host.total_bytes > 0);
        Pool {
            kind: PoolKind::Host,
            device: None,
            total_bytes: host.map(|host| host.total_bytes),
            occupancy: Occupancy {
                measured_bytes: host
                    .and_then(|host| Some(host.total_bytes.saturating_sub(host.available_bytes?))),
                ..Occupancy::default()
            },
        }
    }
}

/// Memory one accelerator device reports.
#[derive(Clone, Debug, PartialEq, Eq)]
struct DeviceMemory {
    device: PoolDevice,
    total_bytes: Option<u64>,
    used_bytes: Option<u64>,
}

impl DeviceMemory {
    /// The device's pool; its size or use is unknown when the device didn't report it.
    fn pool(&self) -> Pool {
        Pool {
            kind: PoolKind::Device,
            device: Some(self.device.clone()),
            total_bytes: self.total_bytes.filter(|total| *total > 0),
            occupancy: Occupancy {
                measured_bytes: self.used_bytes,
                ..Occupancy::default()
            },
        }
    }
}

/// Memory the host reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HostMemory {
    total_bytes: u64,
    available_bytes: Option<u64>,
}

/// Counts in each pool what the workloads hold there: claims of starting workloads first, then
/// what the others hold of the measured use, each segment in workload order.
fn occupy(pools: &mut [Pool], workloads: &[Workload]) {
    for pool in pools {
        // Attribution can be newer than the pool's reading, so what it accounts for never
        // exceeds the use the pool measures. Claims take their share first, so other
        // workloads never change what a pool counts as claimed.
        let mut measured_left = pool.occupancy.measured_bytes.unwrap_or_default();
        let mut segments = Vec::new();
        let starting = workloads
            .iter()
            .enumerate()
            .filter(|(_, workload)| workload.phase == WorkloadPhase::Starting);
        for (index, workload) in starting {
            let Some(share) = workload.claim_in(pool) else {
                // The claim is unknown, but what the node attributes to the workload there is
                // still its own use.
                let (_, attributed) = workload.use_in(pool);
                let measured = attributed.min(measured_left);
                measured_left -= measured;
                pool.occupancy.claimed_bytes = None;
                segments.push((index, workload.segment(SegmentSource::Measured, measured)));
                continue;
            };
            let measured = share
                .loading_bytes
                .min(share.attributed_bytes)
                .min(measured_left);
            measured_left -= measured;
            let bytes = (share.loading_bytes - measured).saturating_add(share.held_bytes);
            pool.occupancy.claimed_bytes = pool
                .occupancy
                .claimed_bytes
                .map(|claimed| claimed.saturating_add(bytes));
            segments.push((index, workload.segment(SegmentSource::Measured, measured)));
            segments.push((index, workload.segment(SegmentSource::Claimed, bytes)));
        }
        let mut running = workloads
            .iter()
            .enumerate()
            .filter(|(_, workload)| workload.phase != WorkloadPhase::Starting)
            .map(|(index, workload)| (index, workload, workload.use_in(pool)))
            .collect::<Vec<_>>();
        // Attributed use is measured, so it takes its share of the reading before estimates.
        running.sort_by_key(|(_, _, (source, _))| *source != SegmentSource::Measured);
        for (index, workload, (source, bytes)) in running {
            let bytes = bytes.min(measured_left);
            measured_left -= bytes;
            segments.push((index, workload.segment(source, bytes)));
        }
        segments.sort_by_key(|(index, _)| *index);
        pool.occupancy.workloads.extend(
            segments
                .into_iter()
                .map(|(_, segment)| segment)
                .filter(|segment| segment.bytes > 0),
        );
    }
}

#[cfg(test)]
mod tests;
