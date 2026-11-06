//! Reconciles runtime platform and node accelerator observations into execution-target inventory.
//!
//! Each result stays attributed to one declared execution target, and unavailable or stale source
//! observations remain explicit rather than being inferred from another target.

use std::{
    collections::BTreeMap,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use dotcore_proto::{
    dotcore::{
        observation::v1::{
            AcceleratorFacts, HardwareSnapshot, accelerator_observation, host_observation,
        },
        site::v1::ExecutionTarget as SiteExecutionTarget,
    },
    google::protobuf::Timestamp,
};

use dotcore_domain::{
    Accelerator, ExecutionPlatform, MemoryArchitecture, NvidiaDriverVersion,
    SelectableAcceleratorDevice, selected_accelerator_facts,
};

/// Availability and freshness of one target-attributed capability observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityObservation<T> {
    /// The observed value is available and fresh.
    Available(T),

    /// The last observed value exists but is no longer fresh enough for admission.
    Stale(T),

    /// No usable value is available from the attributed source.
    Unavailable,
}

impl<T> CapabilityObservation<T> {
    fn map<U>(self, convert: impl FnOnce(T) -> Option<U>) -> CapabilityObservation<U> {
        match self {
            Self::Available(value) => convert(value)
                .map(CapabilityObservation::Available)
                .unwrap_or(CapabilityObservation::Unavailable),
            Self::Stale(value) => convert(value)
                .map(CapabilityObservation::Stale)
                .unwrap_or(CapabilityObservation::Unavailable),
            Self::Unavailable => CapabilityObservation::Unavailable,
        }
    }
}

/// Runtime platform observation attributed to one declared execution target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimePlatformObservation {
    execution_target_id: String,
    observation: CapabilityObservation<ExecutionPlatform>,
    observed_at: Option<SystemTime>,
}

impl RuntimePlatformObservation {
    /// Creates a platform observation for one execution target, stamped now on the system clock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn available(execution_target_id: impl Into<String>, platform: ExecutionPlatform) -> Self {
        Self::available_at(execution_target_id, platform, SystemTime::now())
    }

    /// Creates a timestamped platform observation for one execution target.
    pub fn available_at(
        execution_target_id: impl Into<String>,
        platform: ExecutionPlatform,
        observed_at: SystemTime,
    ) -> Self {
        Self {
            execution_target_id: execution_target_id.into(),
            observation: CapabilityObservation::Available(platform),
            observed_at: Some(observed_at),
        }
    }

    /// Creates a stale platform observation while retaining its last value.
    pub fn stale(execution_target_id: impl Into<String>, platform: ExecutionPlatform) -> Self {
        Self::stale_at(execution_target_id, platform, UNIX_EPOCH)
    }

    /// Creates a stale platform observation with its manager-owned receipt time.
    pub fn stale_at(
        execution_target_id: impl Into<String>,
        platform: ExecutionPlatform,
        observed_at: SystemTime,
    ) -> Self {
        Self {
            execution_target_id: execution_target_id.into(),
            observation: CapabilityObservation::Stale(platform),
            observed_at: Some(observed_at),
        }
    }

    /// Creates an unavailable platform observation for one execution target.
    pub fn unavailable(execution_target_id: impl Into<String>) -> Self {
        Self {
            execution_target_id: execution_target_id.into(),
            observation: CapabilityObservation::Unavailable,
            observed_at: None,
        }
    }
}

/// Node-agent accelerator observation retained with its source managed-node identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAcceleratorObservation {
    managed_node_id: String,
    observation: CapabilityObservation<NodeAcceleratorFacts>,
    observed_at: Option<SystemTime>,
}

/// Accelerator facts of a node's one accelerator vendor: the vendor, the memory architecture its
/// devices share, and the memory facts of each device and of the host, as reported. Explicit
/// constructors report the vendor's usual memory architecture: dedicated for NVIDIA, unified for
/// Apple.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAcceleratorFacts {
    vendor: NodeAcceleratorVendor,
    /// Absent when devices disagree or report no classifiable architecture.
    memory_architecture: Option<MemoryArchitecture>,
    memory: NodeMemoryFacts,
}

/// Memory facts of one node as its observation reported them: each accelerator device of its
/// vendor and its host. Nothing is combined across devices; the capacity component builds memory
/// pools from them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMemoryFacts {
    /// Every device of the node's accelerator vendor, in reported order.
    pub devices: Vec<DeviceMemoryFacts>,

    /// Host memory, absent when the host wasn't observed.
    pub host: Option<HostMemoryFacts>,
}

/// Memory one accelerator device reported.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceMemoryFacts {
    /// Vendor-native immutable identity of the device.
    pub device_id: String,

    /// Product name the vendor reports for the device; empty when unreported.
    pub model: String,

    /// Enumeration index the vendor gives the device, which index selectors name; absent when
    /// unreported.
    pub vendor_index: Option<u32>,

    /// Dedicated memory of the device, absent for unified devices or when unreported.
    pub total_bytes: Option<u64>,

    /// Device-wide used dedicated memory, driver-reserved memory included; absent when unreported.
    pub used_bytes: Option<u64>,
}

impl SelectableAcceleratorDevice for DeviceMemoryFacts {
    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn vendor_index(&self) -> Option<u32> {
        self.vendor_index
    }
}

/// Memory the host reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostMemoryFacts {
    /// Total host memory.
    pub total_bytes: u64,

    /// Memory the host can still hand out, as its operating system reports it.
    pub available_bytes: u64,
}

/// Vendor of a node's accelerators with the facts only that vendor reports.
#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeAcceleratorVendor {
    /// NVIDIA devices with their raw shared driver version.
    Nvidia {
        driver_version: String,
    },

    Apple,
}

impl NodeAcceleratorFacts {
    /// Creates NVIDIA facts with a raw driver version and dedicated memory.
    pub fn nvidia(driver_version: impl Into<String>) -> Self {
        Self {
            vendor: NodeAcceleratorVendor::Nvidia {
                driver_version: driver_version.into(),
            },
            memory_architecture: Some(MemoryArchitecture::Dedicated),
            memory: NodeMemoryFacts::default(),
        }
    }

    /// Creates Apple facts with unified memory.
    pub fn apple() -> Self {
        Self {
            vendor: NodeAcceleratorVendor::Apple,
            memory_architecture: Some(MemoryArchitecture::Unified),
            memory: NodeMemoryFacts::default(),
        }
    }

    fn accelerator(&self) -> Accelerator {
        match self.vendor {
            NodeAcceleratorVendor::Nvidia { .. } => Accelerator::Nvidia,
            NodeAcceleratorVendor::Apple => Accelerator::Apple,
        }
    }

    fn nvidia_driver(&self) -> Option<NvidiaDriverVersion> {
        match &self.vendor {
            NodeAcceleratorVendor::Nvidia { driver_version } => {
                NvidiaDriverVersion::from_str(driver_version).ok()
            }
            NodeAcceleratorVendor::Apple => None,
        }
    }

    /// Adds one accelerator device with its reported dedicated memory total and use, enumerated
    /// after the devices already added.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_device_memory(
        mut self,
        device_id: impl Into<String>,
        total_bytes: Option<u64>,
        used_bytes: Option<u64>,
    ) -> Self {
        let vendor_index = u32::try_from(self.memory.devices.len()).ok();
        self.memory.devices.push(DeviceMemoryFacts {
            device_id: device_id.into(),
            model: String::new(),
            vendor_index,
            total_bytes,
            used_bytes,
        });
        self
    }

    /// Replaces the host memory facts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_host_memory(mut self, total_bytes: u64, available_bytes: u64) -> Self {
        self.memory.host = Some(HostMemoryFacts {
            total_bytes,
            available_bytes,
        });
        self
    }
}

impl NodeAcceleratorObservation {
    /// Attributes the node snapshot's accelerator facts to its managed-node source.
    pub fn from_hardware_snapshot(
        managed_node_id: impl Into<String>,
        snapshot: &HardwareSnapshot,
        stale: bool,
    ) -> Self {
        let observed_at = snapshot
            .observed_at
            .as_ref()
            .and_then(timestamp_to_system_time);
        Self::from_hardware_snapshot_at(managed_node_id, snapshot, stale, observed_at)
    }

    /// Attributes snapshot facts using manager-owned receipt freshness rather than worker wall time.
    pub fn from_hardware_snapshot_at(
        managed_node_id: impl Into<String>,
        snapshot: &HardwareSnapshot,
        stale: bool,
        observed_at: Option<SystemTime>,
    ) -> Self {
        let managed_node_id = managed_node_id.into();
        let facts = NodeMemoryReading::from_snapshot(snapshot).accelerator_facts();
        match (facts, stale, observed_at) {
            (Some(facts), false, Some(observed_at)) => {
                Self::available_at(managed_node_id, facts, observed_at)
            }
            (Some(facts), _, Some(observed_at)) => Self {
                managed_node_id,
                observation: CapabilityObservation::Stale(facts),
                observed_at: Some(observed_at),
            },
            _ => Self::unavailable(managed_node_id),
        }
    }

    /// Creates an NVIDIA observation with a raw driver version for one managed node, stamped now
    /// on the system clock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn nvidia(managed_node_id: impl Into<String>, driver_version: impl Into<String>) -> Self {
        Self::available(
            managed_node_id,
            NodeAcceleratorFacts::nvidia(driver_version),
        )
    }

    /// Creates an accelerator observation for one managed node, stamped now on the system clock.
    #[cfg(any(test, feature = "test-support"))]
    pub fn available(managed_node_id: impl Into<String>, facts: NodeAcceleratorFacts) -> Self {
        Self::available_at(managed_node_id, facts, SystemTime::now())
    }

    /// Creates a timestamped accelerator observation for one managed node.
    pub fn available_at(
        managed_node_id: impl Into<String>,
        facts: NodeAcceleratorFacts,
        observed_at: SystemTime,
    ) -> Self {
        Self {
            managed_node_id: managed_node_id.into(),
            observation: CapabilityObservation::Available(facts),
            observed_at: Some(observed_at),
        }
    }

    /// Creates a stale accelerator observation while retaining its last value.
    pub fn stale(managed_node_id: impl Into<String>, facts: NodeAcceleratorFacts) -> Self {
        Self {
            managed_node_id: managed_node_id.into(),
            observation: CapabilityObservation::Stale(facts),
            observed_at: Some(UNIX_EPOCH),
        }
    }

    /// Replaces the observed memory architecture, where absent means devices could not agree.
    pub fn with_memory_architecture(
        mut self,
        memory_architecture: Option<MemoryArchitecture>,
    ) -> Self {
        match &mut self.observation {
            CapabilityObservation::Available(facts) | CapabilityObservation::Stale(facts) => {
                facts.memory_architecture = memory_architecture;
            }
            CapabilityObservation::Unavailable => {}
        }
        self
    }

    /// Creates an unavailable accelerator observation for one managed node.
    pub fn unavailable(managed_node_id: impl Into<String>) -> Self {
        Self {
            managed_node_id: managed_node_id.into(),
            observation: CapabilityObservation::Unavailable,
            observed_at: None,
        }
    }

    /// Adds one accelerator device with its reported dedicated memory total and use.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_device_memory(
        self,
        device_id: impl Into<String>,
        total_bytes: Option<u64>,
        used_bytes: Option<u64>,
    ) -> Self {
        self.map_facts(|facts| facts.with_device_memory(device_id, total_bytes, used_bytes))
    }

    /// Replaces the host memory facts.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_host_memory(self, total_bytes: u64, available_bytes: u64) -> Self {
        self.map_facts(|facts| facts.with_host_memory(total_bytes, available_bytes))
    }

    #[cfg(any(test, feature = "test-support"))]
    fn map_facts(
        mut self,
        update: impl FnOnce(NodeAcceleratorFacts) -> NodeAcceleratorFacts,
    ) -> Self {
        self.observation = match self.observation {
            CapabilityObservation::Available(facts) => {
                CapabilityObservation::Available(update(facts))
            }
            CapabilityObservation::Stale(facts) => CapabilityObservation::Stale(update(facts)),
            CapabilityObservation::Unavailable => CapabilityObservation::Unavailable,
        };
        self
    }
}

/// One node's memory as its latest report gives it, attributed to the node whether or not any of
/// its execution targets can use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeMemoryObservation {
    /// The node whose report gave the memory.
    pub site_node_id: String,

    /// The reported memory, stale once the report is no longer fresh.
    pub reading: CapabilityObservation<NodeMemoryReading>,

    /// When the report was received; absent when the node never reported its hardware.
    pub observed_at: Option<SystemTime>,
}

impl NodeMemoryObservation {
    /// Reads the memory of a node's hardware snapshot, received at `observed_at`; a missing
    /// receipt time reads as unavailable.
    pub fn from_hardware_snapshot_at(
        site_node_id: impl Into<String>,
        snapshot: &HardwareSnapshot,
        stale: bool,
        observed_at: Option<SystemTime>,
    ) -> Self {
        let site_node_id = site_node_id.into();
        let Some(observed_at) = observed_at else {
            return Self::unavailable(site_node_id);
        };
        let reading = NodeMemoryReading::from_snapshot(snapshot);
        Self {
            site_node_id,
            reading: if stale {
                CapabilityObservation::Stale(reading)
            } else {
                CapabilityObservation::Available(reading)
            },
            observed_at: Some(observed_at),
        }
    }

    /// Reads the memory of a node's hardware snapshot at the time the snapshot names.
    #[cfg(any(test, feature = "test-support"))]
    pub fn from_hardware_snapshot(
        site_node_id: impl Into<String>,
        snapshot: &HardwareSnapshot,
        stale: bool,
    ) -> Self {
        let observed_at = snapshot
            .observed_at
            .as_ref()
            .and_then(timestamp_to_system_time);
        Self::from_hardware_snapshot_at(site_node_id, snapshot, stale, observed_at)
    }

    /// A node whose memory can't be read: it never reported, or its report holds no hardware.
    pub fn unavailable(site_node_id: impl Into<String>) -> Self {
        Self {
            site_node_id: site_node_id.into(),
            reading: CapabilityObservation::Unavailable,
            observed_at: None,
        }
    }
}

/// Memory of one node as its report gives it: every accelerator device of every vendor, vendors in
/// reported order and each vendor's devices in its order, with each device's own memory
/// architecture, and the host. Target memory facts are this reading narrowed to the vendor a
/// target can select.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeMemoryReading {
    vendors: Vec<VendorMemoryReading>,
    host: Option<HostMemoryFacts>,
}

/// Devices one vendor reported on a node.
#[derive(Clone, Debug, PartialEq, Eq)]
struct VendorMemoryReading {
    /// Absent for a vendor no execution target can select.
    vendor: Option<Accelerator>,
    devices: Vec<DeviceMemoryReading>,
}

/// Memory one accelerator device reported, with the architecture that decides where it lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceMemoryReading {
    /// Absent when the device reports no classifiable architecture.
    pub memory_architecture: Option<MemoryArchitecture>,

    pub memory: DeviceMemoryFacts,

    /// Driver or runtime version the device reports, which target facts require to agree.
    driver_version: String,
}

impl NodeMemoryReading {
    /// Every accelerator device the node reported, vendor by vendor.
    pub fn devices(&self) -> impl Iterator<Item = &DeviceMemoryReading> {
        self.vendors.iter().flat_map(|vendor| &vendor.devices)
    }

    /// Host memory, absent when the host wasn't observed.
    pub fn host(&self) -> Option<HostMemoryFacts> {
        self.host
    }

    /// Reads every vendor's devices and the host from a node's hardware snapshot.
    fn from_snapshot(snapshot: &HardwareSnapshot) -> Self {
        let vendors = snapshot
            .accelerators
            .iter()
            .filter_map(|observation| match observation.result.as_ref()? {
                accelerator_observation::Result::Facts(set) => Some(VendorMemoryReading {
                    vendor: Accelerator::from_proto(observation.vendor),
                    devices: set.facts.iter().map(device_memory_reading).collect(),
                }),
                accelerator_observation::Result::Unavailable(_) => None,
            })
            .collect();
        Self {
            vendors,
            host: host_memory_facts(snapshot),
        }
    }

    /// Projects the facts of the node's one selectable accelerator vendor. A vendor counts only
    /// when it reports devices, NVIDIA additionally needs one driver version shared by every
    /// device, and a node reporting devices from both selectable vendors is ambiguous.
    fn accelerator_facts(self) -> Option<NodeAcceleratorFacts> {
        let mut vendors = self.vendors.into_iter().filter_map(|reading| {
            let vendor = reading.vendor?;
            (!reading.devices.is_empty()).then_some((vendor, reading.devices))
        });
        let (accelerator, devices) = vendors.next()?;
        if vendors.next().is_some() {
            return None;
        }
        let vendor = match accelerator {
            Accelerator::Nvidia => {
                let mut drivers = devices.iter().map(|device| device.driver_version.as_str());
                let first = drivers.next().filter(|driver| !driver.is_empty())?;
                let driver_version = drivers.all(|driver| driver == first).then_some(first)?;
                NodeAcceleratorVendor::Nvidia {
                    driver_version: driver_version.to_owned(),
                }
            }
            Accelerator::Apple => NodeAcceleratorVendor::Apple,
        };
        Some(NodeAcceleratorFacts {
            vendor,
            memory_architecture: shared_memory_architecture(&devices),
            memory: NodeMemoryFacts {
                devices: devices.into_iter().map(|device| device.memory).collect(),
                host: self.host,
            },
        })
    }
}

/// Reconciled capability observations for one declared execution target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetCapabilities {
    /// Declared execution target to which every observation is attributed.
    pub execution_target_id: String,
    /// Declared managed node supplying the accelerator observation.
    pub managed_node_id: String,
    /// Runtime-reported effective execution platform.
    pub platform: CapabilityObservation<ExecutionPlatform>,
    /// Time at which the runtime platform was observed.
    pub platform_observed_at: Option<SystemTime>,
    /// Node-agent-reported vendor of the node's accelerators.
    pub accelerator: CapabilityObservation<Accelerator>,
    /// Time at which the node's accelerators were observed; every accelerator fact shares it.
    pub accelerator_observed_at: Option<SystemTime>,
    /// Node-agent-reported NVIDIA driver version, unavailable for other vendors.
    pub nvidia_driver: CapabilityObservation<NvidiaDriverVersion>,
    /// Node-agent-reported memory architecture shared by every accelerator device on the node.
    pub accelerator_memory: CapabilityObservation<MemoryArchitecture>,
    /// Node-agent-reported memory of each accelerator device and of the host, from the same
    /// observation; the capacity component builds memory pools from it.
    pub memory: CapabilityObservation<NodeMemoryFacts>,
    /// Vendor-native ids of the node's devices the target's accelerator selectors name, in reported
    /// order; absent when the node reported no device facts or a selector names no single device.
    pub accelerator_device_ids: Option<Vec<String>>,
}

/// Site-manager-owned current inventory keyed by declared execution-target identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetCapabilityInventory {
    targets: BTreeMap<String, TargetCapabilities>,
}

impl TargetCapabilityInventory {
    /// Returns capability observations only for the exact execution target requested.
    pub fn for_target(&self, execution_target_id: &str) -> Option<&TargetCapabilities> {
        self.targets.get(execution_target_id)
    }

    /// Combines target observations that were reconciled under separate runtime capabilities.
    pub fn extend(&mut self, other: Self) {
        for (target_id, capabilities) in other.targets {
            assert!(
                self.targets.insert(target_id, capabilities).is_none(),
                "execution target identities are unique after site validation"
            );
        }
    }

    /// Iterates target-attributed observations in stable target-ID order.
    pub fn targets(&self) -> impl Iterator<Item = &TargetCapabilities> {
        self.targets.values()
    }
}
/// Refreshes site-manager-owned target capability observations on demand.
pub trait TargetCapabilitySource: Send + Sync {
    /// Collects one timestamped inventory from the declared runtime and node sources.
    fn observe(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<TargetCapabilityInventory, TargetCapabilitySourceError>,
                > + Send
                + '_,
        >,
    >;
}

/// Failure while refreshing target-attributed capability observations.
#[derive(Debug, thiserror::Error)]
pub enum TargetCapabilitySourceError {
    #[error("target capability observation failed: {0}")]
    Observation(#[source] anyhow::Error),
}

/// Deterministic capability source for compositions whose observations are explicitly supplied.
#[derive(Clone, Default)]
pub struct StaticTargetCapabilitySource {
    inventory: TargetCapabilityInventory,
}

impl StaticTargetCapabilitySource {
    /// Creates a source that returns the supplied immutable inventory.
    pub fn new(inventory: TargetCapabilityInventory) -> Self {
        Self { inventory }
    }
}

impl TargetCapabilitySource for StaticTargetCapabilitySource {
    fn observe(
        &self,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<TargetCapabilityInventory, TargetCapabilitySourceError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(std::future::ready(Ok(self.inventory.clone())))
    }
}

/// Stateless site-manager reconciliation of runtime and node-agent observations.
pub struct TargetCapabilityReconciler;

impl TargetCapabilityReconciler {
    /// Joins observations only through each target's declared target and managed-node identities.
    pub fn reconcile<T, R, N>(
        targets: T,
        runtime_platforms: R,
        node_accelerators: N,
    ) -> TargetCapabilityInventory
    where
        T: IntoIterator,
        T::Item: TargetIdentity,
        R: IntoIterator<Item = RuntimePlatformObservation>,
        N: IntoIterator<Item = NodeAcceleratorObservation>,
    {
        let mut platforms = BTreeMap::new();
        for observation in runtime_platforms {
            insert_unambiguous_observation(
                &mut platforms,
                observation.execution_target_id,
                (observation.observation, observation.observed_at),
            );
        }
        let mut accelerators = BTreeMap::new();
        for observation in node_accelerators {
            insert_unambiguous_observation(
                &mut accelerators,
                observation.managed_node_id,
                (observation.observation, observation.observed_at),
            );
        }
        let targets = targets
            .into_iter()
            .map(|target| {
                let (execution_target_id, managed_node_id, selectors) = target.into_identity();
                let (platform, platform_observed_at) = platforms
                    .get(&execution_target_id)
                    .cloned()
                    .unwrap_or((CapabilityObservation::Unavailable, None));
                let (facts, accelerator_observed_at) = accelerators
                    .get(&managed_node_id)
                    .cloned()
                    .unwrap_or((CapabilityObservation::Unavailable, None));
                let accelerator = facts.clone().map(|facts| Some(facts.accelerator()));
                let nvidia_driver = facts.clone().map(|facts| facts.nvidia_driver());
                let accelerator_device_ids = match &facts {
                    CapabilityObservation::Available(facts)
                    | CapabilityObservation::Stale(facts) => {
                        selected_accelerator_facts(&selectors, &facts.memory.devices).map(
                            |devices| {
                                devices
                                    .into_iter()
                                    .map(|device| device.device_id.clone())
                                    .collect()
                            },
                        )
                    }
                    CapabilityObservation::Unavailable => None,
                };
                let memory = facts.clone().map(|facts| Some(facts.memory));
                let accelerator_memory = facts.map(|facts| facts.memory_architecture);
                let capability = TargetCapabilities {
                    execution_target_id: execution_target_id.clone(),
                    managed_node_id,
                    platform,
                    platform_observed_at,
                    accelerator,
                    accelerator_observed_at,
                    nvidia_driver,
                    accelerator_memory,
                    memory,
                    accelerator_device_ids,
                };
                (execution_target_id, capability)
            })
            .collect();
        TargetCapabilityInventory { targets }
    }
}

/// Reads one device's reported memory and memory architecture.
fn device_memory_reading(fact: &AcceleratorFacts) -> DeviceMemoryReading {
    DeviceMemoryReading {
        memory_architecture: MemoryArchitecture::from_proto(fact.memory_architecture()),
        memory: DeviceMemoryFacts {
            device_id: fact.device_id.clone(),
            model: fact.model.clone(),
            vendor_index: fact.vendor_index,
            total_bytes: fact.total_memory_bytes,
            used_bytes: fact.used_memory_bytes,
        },
        driver_version: fact.driver_or_runtime_version.clone(),
    }
}

/// Returns the architecture every device reports, or none when any device disagrees or is unclassified.
fn shared_memory_architecture(devices: &[DeviceMemoryReading]) -> Option<MemoryArchitecture> {
    let mut architectures = devices.iter().map(|device| device.memory_architecture);
    let first = architectures.next()??;
    architectures
        .all(|architecture| architecture == Some(first))
        .then_some(first)
}

/// Returns the host's memory facts, or none when the host wasn't observed.
fn host_memory_facts(snapshot: &HardwareSnapshot) -> Option<HostMemoryFacts> {
    match snapshot.host.as_ref()?.result.as_ref()? {
        host_observation::Result::Facts(host) => Some(HostMemoryFacts {
            total_bytes: host.total_memory_bytes,
            available_bytes: host.available_memory_bytes,
        }),
        host_observation::Result::Unavailable(_) => None,
    }
}

pub fn timestamp_to_system_time(timestamp: &Timestamp) -> Option<SystemTime> {
    let seconds = u64::try_from(timestamp.seconds).ok()?;
    let nanos = u32::try_from(timestamp.nanos)
        .ok()
        .filter(|nanos| *nanos < 1_000_000_000)?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))?
        .checked_add(Duration::from_nanos(u64::from(nanos)))
}

/// Whether a capability observation taken at `observed_at` is still young enough at `now` for
/// deployment admission or launch, under the production capability age.
pub fn observation_is_fresh(observed_at: Option<SystemTime>, now: SystemTime) -> bool {
    observed_at
        .and_then(|observed_at| now.duration_since(observed_at).ok())
        .is_some_and(|age| {
            age <= dotcore_domain::timing::Timing::production().target_capability_max_age
        })
}

fn insert_unambiguous_observation<T>(
    observations: &mut BTreeMap<String, (CapabilityObservation<T>, Option<SystemTime>)>,
    source_id: String,
    observation: (CapabilityObservation<T>, Option<SystemTime>),
) {
    use std::collections::btree_map::Entry;

    match observations.entry(source_id) {
        Entry::Vacant(entry) => {
            entry.insert(observation);
        }
        Entry::Occupied(mut entry) => {
            entry.insert((CapabilityObservation::Unavailable, None));
        }
    }
}

/// Converts a configured execution-target identity into the reconciliation join keys and the
/// accelerator selectors that map the target to its node's devices.
pub trait TargetIdentity {
    /// Returns the execution-target and managed-node identifiers used for reconciliation, and the
    /// target's accelerator selectors.
    fn into_identity(self) -> (String, String, Vec<String>);
}

impl TargetIdentity for &SiteExecutionTarget {
    fn into_identity(self) -> (String, String, Vec<String>) {
        (
            self.execution_target_id.clone(),
            self.site_node_id.clone(),
            self.accelerator_device_ids.clone(),
        )
    }
}

/// A target that selects no accelerator device.
impl<A, B> TargetIdentity for (A, B)
where
    A: Into<String>,
    B: Into<String>,
{
    fn into_identity(self) -> (String, String, Vec<String>) {
        (self.0.into(), self.1.into(), Vec::new())
    }
}

/// A target with its accelerator selectors.
impl<A, B, C> TargetIdentity for (A, B, C)
where
    A: Into<String>,
    B: Into<String>,
    C: IntoIterator,
    C::Item: Into<String>,
{
    fn into_identity(self) -> (String, String, Vec<String>) {
        let selectors = self.2.into_iter().map(Into::into).collect();
        (self.0.into(), self.1.into(), selectors)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use dotcore_domain::{MemoryArchitecture, NvidiaDriverVersion};
    use dotcore_proto::dotcore::observation::v1::{
        AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
        AcceleratorObservation, AcceleratorVendor, HardwareSnapshot, HostFacts, HostObservation,
        ObservationUnavailable, accelerator_observation, host_observation,
    };

    use super::{
        Accelerator, CapabilityObservation, DeviceMemoryFacts, ExecutionPlatform, HostMemoryFacts,
        NodeAcceleratorFacts, NodeAcceleratorObservation, NodeMemoryFacts, NodeMemoryObservation,
        RuntimePlatformObservation, TargetCapabilityReconciler,
    };

    #[test]
    fn reconciliation_never_combines_observations_from_different_targets() {
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a"), ("target-b", "node-b")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_arm64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-b", "580.173.02")],
        );

        assert_eq!(
            inventory.for_target("target-a").expect("target a").platform,
            CapabilityObservation::Available(ExecutionPlatform::linux_arm64())
        );
        assert_eq!(
            inventory
                .for_target("target-a")
                .expect("target a")
                .nvidia_driver,
            CapabilityObservation::Unavailable
        );
        assert_eq!(
            inventory.for_target("target-b").expect("target b").platform,
            CapabilityObservation::Unavailable
        );
        assert_eq!(
            inventory
                .for_target("target-b")
                .expect("target b")
                .nvidia_driver,
            CapabilityObservation::Available(
                NvidiaDriverVersion::from_str("580.173.02").expect("driver version")
            )
        );
    }

    #[test]
    fn stale_and_unavailable_observations_remain_explicit() {
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::stale(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::unavailable("node-a")],
        );
        let target = inventory.for_target("target-a").expect("target a");

        assert_eq!(
            target.platform,
            CapabilityObservation::Stale(ExecutionPlatform::linux_amd64())
        );
        assert_eq!(target.nvidia_driver, CapabilityObservation::Unavailable);
    }

    #[test]
    fn node_agent_snapshot_preserves_driver_freshness_when_attributed_to_a_target() {
        let snapshot = HardwareSnapshot {
            observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 1_725_000_000,
                nanos: 0,
            }),
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Nvidia as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet {
                        facts: vec![AcceleratorFacts {
                            vendor: AcceleratorVendor::Nvidia as i32,
                            driver_or_runtime_version: "580.173.02".to_owned(),
                            ..Default::default()
                        }],
                    },
                )),
            }],
            ..Default::default()
        };
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_arm64(),
            )],
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-a", &snapshot, true,
            )],
        );

        assert_eq!(
            inventory
                .for_target("target-a")
                .expect("target a")
                .nvidia_driver,
            CapabilityObservation::Stale(
                NvidiaDriverVersion::from_str("580.173.02").expect("driver")
            )
        );
    }

    #[test]
    fn node_snapshot_memory_architecture_requires_every_nvidia_device_to_agree() {
        use AcceleratorMemoryArchitecture::{Dedicated, Unified, Unspecified};

        let nodes = [
            ("node-dedicated", vec![Dedicated, Dedicated], false),
            ("node-unified", vec![Unified], false),
            ("node-mixed", vec![Dedicated, Unified], false),
            ("node-unspecified", vec![Unspecified], false),
            ("node-stale", vec![Unified], true),
        ];
        let inventory = TargetCapabilityReconciler::reconcile(
            nodes
                .iter()
                .map(|(node, _, _)| (format!("target-{node}"), *node)),
            std::iter::empty(),
            nodes.iter().map(|(node, architectures, stale)| {
                NodeAcceleratorObservation::from_hardware_snapshot(
                    *node,
                    &nvidia_snapshot(architectures),
                    *stale,
                )
            }),
        );
        let memory = |node: &str| {
            inventory
                .for_target(&format!("target-{node}"))
                .expect("target")
                .accelerator_memory
                .clone()
        };

        assert_eq!(
            memory("node-dedicated"),
            CapabilityObservation::Available(MemoryArchitecture::Dedicated)
        );
        assert_eq!(
            memory("node-unified"),
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
        assert_eq!(memory("node-mixed"), CapabilityObservation::Unavailable);
        assert_eq!(
            memory("node-unspecified"),
            CapabilityObservation::Unavailable
        );
        assert_eq!(
            memory("node-stale"),
            CapabilityObservation::Stale(MemoryArchitecture::Unified)
        );
        assert_eq!(
            inventory
                .for_target("target-node-mixed")
                .expect("mixed target")
                .nvidia_driver,
            CapabilityObservation::Available(
                NvidiaDriverVersion::from_str("580.173.02").expect("driver")
            )
        );
    }

    #[test]
    fn node_snapshot_passes_each_device_and_host_memory_fact_through() {
        use AcceleratorMemoryArchitecture::Dedicated;

        const GIB: u64 = 1024 * 1024 * 1024;
        let mut snapshot = memory_snapshot(
            &[(Dedicated, Some(24 * GIB)), (Dedicated, Some(16 * GIB))],
            Some(64 * GIB),
        );
        let Some(accelerator_observation::Result::Facts(set)) =
            snapshot.accelerators[0].result.as_mut()
        else {
            unreachable!("NVIDIA snapshot has facts");
        };
        set.facts[0].device_id = "GPU-a".to_owned();
        set.facts[0].used_memory_bytes = Some(GIB);
        set.facts[1].device_id = "GPU-b".to_owned();
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            std::iter::empty(),
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-a", &snapshot, false,
            )],
        );

        assert_eq!(
            inventory.for_target("target-a").expect("target").memory,
            CapabilityObservation::Available(NodeMemoryFacts {
                devices: vec![
                    DeviceMemoryFacts {
                        device_id: "GPU-a".to_owned(),
                        model: String::new(),
                        vendor_index: None,
                        total_bytes: Some(24 * GIB),
                        used_bytes: Some(GIB),
                    },
                    DeviceMemoryFacts {
                        device_id: "GPU-b".to_owned(),
                        model: String::new(),
                        vendor_index: None,
                        total_bytes: Some(16 * GIB),
                        used_bytes: None,
                    },
                ],
                host: Some(HostMemoryFacts {
                    total_bytes: 64 * GIB,
                    available_bytes: 0,
                }),
            })
        );
    }

    #[test]
    fn each_target_maps_to_the_node_devices_its_selectors_name() {
        let node = NodeAcceleratorFacts::nvidia("580.173.02")
            .with_device_memory("GPU-a", Some(24), Some(1))
            .with_device_memory("GPU-b", Some(16), Some(1));
        let inventory = TargetCapabilityReconciler::reconcile(
            [
                ("by-index", "node-a", ["nvidia.com/gpu=1"]),
                ("by-device-id", "node-a", ["GPU-a"]),
                ("missing", "node-a", ["nvidia.com/gpu=2"]),
                ("absent-node", "node-b", ["GPU-a"]),
            ],
            std::iter::empty(),
            [
                NodeAcceleratorObservation::available("node-a", node.clone()),
                NodeAcceleratorObservation::unavailable("node-b"),
            ],
        );
        let mut both = TargetCapabilityReconciler::reconcile(
            [("both", "node-a", ["GPU-b", "nvidia.com/gpu=0"])],
            std::iter::empty(),
            [NodeAcceleratorObservation::stale("node-a", node.clone())],
        );
        both.extend(TargetCapabilityReconciler::reconcile(
            [("none", "node-a")],
            std::iter::empty(),
            [NodeAcceleratorObservation::available("node-a", node)],
        ));
        let devices = |inventory: &super::TargetCapabilityInventory, target: &str| {
            inventory
                .for_target(target)
                .expect("target")
                .accelerator_device_ids
                .clone()
        };

        assert_eq!(
            devices(&inventory, "by-index"),
            Some(vec!["GPU-b".to_owned()])
        );
        assert_eq!(
            devices(&inventory, "by-device-id"),
            Some(vec!["GPU-a".to_owned()])
        );
        assert_eq!(devices(&inventory, "missing"), None);
        assert_eq!(devices(&inventory, "absent-node"), None);
        // A stale observation still maps its devices, in reported order.
        assert_eq!(
            devices(&both, "both"),
            Some(vec!["GPU-a".to_owned(), "GPU-b".to_owned()])
        );
        assert_eq!(devices(&both, "none"), Some(Vec::new()));
    }

    #[test]
    fn apple_snapshot_reports_a_unified_pool_without_a_driver() {
        const HOST_TOTAL: u64 = 68_719_476_736;
        let mut snapshot = memory_snapshot(&[], Some(HOST_TOTAL));
        snapshot.accelerators = vec![vendor_observation(
            AcceleratorVendor::Apple,
            AcceleratorMemoryArchitecture::Unified,
        )];
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            std::iter::empty(),
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-a", &snapshot, false,
            )],
        );
        let target = inventory.for_target("target-a").expect("target a");

        assert_eq!(
            target.accelerator,
            CapabilityObservation::Available(Accelerator::Apple)
        );
        assert_eq!(target.nvidia_driver, CapabilityObservation::Unavailable);
        assert_eq!(
            target.accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
        assert_eq!(
            target.memory,
            CapabilityObservation::Available(NodeMemoryFacts {
                devices: vec![DeviceMemoryFacts {
                    device_id: String::new(),
                    model: String::new(),
                    vendor_index: None,
                    total_bytes: None,
                    used_bytes: None,
                }],
                host: Some(HostMemoryFacts {
                    total_bytes: HOST_TOTAL,
                    available_bytes: 0,
                }),
            })
        );
    }

    #[test]
    fn snapshot_vendor_ignores_unselectable_vendors_and_rejects_two_selectable_ones() {
        use AcceleratorMemoryArchitecture::{Dedicated, Unified};

        let mut with_amd = nvidia_snapshot(&[Dedicated]);
        with_amd
            .accelerators
            .push(vendor_observation(AcceleratorVendor::Amd, Dedicated));
        let mut with_apple = nvidia_snapshot(&[Dedicated]);
        with_apple
            .accelerators
            .push(vendor_observation(AcceleratorVendor::Apple, Unified));
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-amd", "node-amd"), ("target-both", "node-both")],
            std::iter::empty(),
            [
                NodeAcceleratorObservation::from_hardware_snapshot("node-amd", &with_amd, false),
                NodeAcceleratorObservation::from_hardware_snapshot("node-both", &with_apple, false),
            ],
        );
        let accelerator = |target: &str| {
            inventory
                .for_target(target)
                .expect("target")
                .accelerator
                .clone()
        };

        assert_eq!(
            accelerator("target-amd"),
            CapabilityObservation::Available(Accelerator::Nvidia)
        );
        assert_eq!(
            accelerator("target-both"),
            CapabilityObservation::Unavailable
        );
    }

    #[test]
    fn node_memory_reads_every_vendors_devices_where_no_target_can_select_them() {
        use AcceleratorMemoryArchitecture::{Dedicated, Unified, Unspecified};

        const GIB: u64 = 1024 * 1024 * 1024;
        let mut mixed = memory_snapshot(
            &[(Dedicated, Some(24 * GIB)), (Unspecified, Some(16 * GIB))],
            Some(64 * GIB),
        );
        let Some(accelerator_observation::Result::Facts(set)) =
            mixed.accelerators[0].result.as_mut()
        else {
            unreachable!("NVIDIA snapshot has facts");
        };
        set.facts[0].device_id = "GPU-a".to_owned();
        set.facts[1].device_id = "GPU-b".to_owned();
        set.facts[1].driver_or_runtime_version = "575.64.05".to_owned();
        mixed
            .accelerators
            .push(vendor_observation(AcceleratorVendor::Amd, Unified));
        let host_only = memory_snapshot(&[], Some(64 * GIB));
        let host_only = HardwareSnapshot {
            accelerators: Vec::new(),
            ..host_only
        };
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-mixed", "node-mixed")],
            std::iter::empty(),
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-mixed",
                &mixed,
                false,
            )],
        );
        let devices = |observation: &NodeMemoryObservation| {
            let CapabilityObservation::Available(reading) = &observation.reading else {
                panic!("available reading");
            };
            reading
                .devices()
                .map(|device| (device.memory_architecture, device.memory.clone()))
                .collect::<Vec<_>>()
        };
        let device = |device_id: &str, total_bytes| DeviceMemoryFacts {
            device_id: device_id.to_owned(),
            model: String::new(),
            vendor_index: None,
            total_bytes,
            used_bytes: None,
        };

        // Drivers that disagree leave the node's target unavailable, while its memory reads.
        assert_eq!(
            inventory.for_target("target-mixed").expect("target").memory,
            CapabilityObservation::Unavailable
        );
        let mixed = NodeMemoryObservation::from_hardware_snapshot("node-mixed", &mixed, false);
        assert_eq!(
            devices(&mixed),
            vec![
                (
                    Some(MemoryArchitecture::Dedicated),
                    device("GPU-a", Some(24 * GIB))
                ),
                (None, device("GPU-b", Some(16 * GIB))),
                (Some(MemoryArchitecture::Unified), device("", None)),
            ]
        );
        let host_only =
            NodeMemoryObservation::from_hardware_snapshot("node-host", &host_only, true);
        let CapabilityObservation::Stale(reading) = &host_only.reading else {
            panic!("stale reading");
        };
        assert_eq!(reading.devices().count(), 0);
        assert_eq!(
            reading.host(),
            Some(HostMemoryFacts {
                total_bytes: 64 * GIB,
                available_bytes: 0,
            })
        );
        assert!(host_only.observed_at.is_some());
    }

    #[test]
    fn explicit_node_observations_default_to_dedicated_memory() {
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a"), ("target-b", "node-b")],
            std::iter::empty(),
            [
                NodeAcceleratorObservation::nvidia("node-a", "580"),
                NodeAcceleratorObservation::nvidia("node-b", "580")
                    .with_memory_architecture(Some(MemoryArchitecture::Unified)),
            ],
        );

        assert_eq!(
            inventory
                .for_target("target-a")
                .expect("target a")
                .accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Dedicated)
        );
        assert_eq!(
            inventory
                .for_target("target-b")
                .expect("target b")
                .accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
    }

    #[test]
    fn malformed_or_missing_driver_facts_remain_unavailable() {
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a"), ("target-b", "node-b")],
            std::iter::empty(),
            [NodeAcceleratorObservation::nvidia("node-a", "driver-580")],
        );

        assert_eq!(
            inventory
                .for_target("target-a")
                .expect("target a")
                .nvidia_driver,
            CapabilityObservation::Unavailable
        );
        assert_eq!(
            inventory
                .for_target("target-b")
                .expect("target b")
                .nvidia_driver,
            CapabilityObservation::Unavailable
        );
    }

    fn memory_snapshot(
        devices: &[(AcceleratorMemoryArchitecture, Option<u64>)],
        host_total: Option<u64>,
    ) -> HardwareSnapshot {
        let architectures = devices
            .iter()
            .map(|(architecture, _)| *architecture)
            .collect::<Vec<_>>();
        let mut snapshot = nvidia_snapshot(&architectures);
        let Some(accelerator_observation::Result::Facts(set)) =
            snapshot.accelerators[0].result.as_mut()
        else {
            unreachable!("NVIDIA snapshot has facts");
        };
        for (fact, (_, total)) in set.facts.iter_mut().zip(devices) {
            fact.total_memory_bytes = *total;
        }
        snapshot.host = Some(HostObservation {
            result: Some(match host_total {
                Some(total_memory_bytes) => host_observation::Result::Facts(HostFacts {
                    total_memory_bytes,
                    ..Default::default()
                }),
                None => host_observation::Result::Unavailable(ObservationUnavailable::default()),
            }),
        });
        snapshot
    }

    fn vendor_observation(
        vendor: AcceleratorVendor,
        architecture: AcceleratorMemoryArchitecture,
    ) -> AcceleratorObservation {
        AcceleratorObservation {
            vendor: vendor as i32,
            result: Some(accelerator_observation::Result::Facts(
                AcceleratorFactsSet {
                    facts: vec![AcceleratorFacts {
                        vendor: vendor as i32,
                        memory_architecture: architecture as i32,
                        ..Default::default()
                    }],
                },
            )),
        }
    }

    fn nvidia_snapshot(architectures: &[AcceleratorMemoryArchitecture]) -> HardwareSnapshot {
        HardwareSnapshot {
            observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 1_725_000_000,
                nanos: 0,
            }),
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Nvidia as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet {
                        facts: architectures
                            .iter()
                            .map(|architecture| AcceleratorFacts {
                                vendor: AcceleratorVendor::Nvidia as i32,
                                memory_architecture: *architecture as i32,
                                driver_or_runtime_version: "580.173.02".to_owned(),
                                ..Default::default()
                            })
                            .collect(),
                    },
                )),
            }],
            ..Default::default()
        }
    }
}
