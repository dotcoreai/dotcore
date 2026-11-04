//! Node accelerator facts shared by catalog compatibility, site inventory, and placement: the
//! accelerator vendor, the NVIDIA driver version a node reports, an engine artifact's minimum
//! driver major version, and whether accelerators own dedicated memory or share the host memory
//! pool, how a container runtime selects accelerator devices (CDI device names or NVIDIA
//! device indexes), and which observed hardware devices those selectors name.
//!
//! For example, a node reporting driver `580.173.02` satisfies a minimum major version of `580`,
//! and the selector `nvidia.com/gpu=1` names the device NVML enumerates second.

use std::{collections::BTreeSet, num::NonZeroU32, str::FromStr};

use dotcore_proto::dotcore::{
    observation::v1::{
        AcceleratorFacts, AcceleratorMemoryArchitecture, AcceleratorVendor, HardwareSnapshot,
        accelerator_observation,
    },
    site::v1::{AcceleratorResolutionStatus, DockerAcceleratorTransport},
};

/// Accelerator vendor that engine artifact selectors and model hardware requirements name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accelerator {
    /// NVIDIA accelerators, whose engine artifacts declare a minimum driver major version.
    Nvidia,

    /// Apple Silicon GPUs, which allocate from the node's unified memory pool.
    Apple,
}

impl Accelerator {
    /// Parses the catalog vendor name, such as `nvidia` or `apple`.
    pub fn from_catalog_name(value: &str) -> Option<Self> {
        match value {
            "nvidia" => Some(Self::Nvidia),
            "apple" => Some(Self::Apple),
            _ => None,
        }
    }

    /// Converts an observed protobuf vendor, or none for a vendor engine artifacts cannot name.
    pub fn from_proto(vendor: i32) -> Option<Self> {
        match AcceleratorVendor::try_from(vendor).ok()? {
            AcceleratorVendor::Nvidia => Some(Self::Nvidia),
            AcceleratorVendor::Apple => Some(Self::Apple),
            AcceleratorVendor::Amd | AcceleratorVendor::Unspecified => None,
        }
    }

    /// Returns the catalog vendor name.
    pub const fn catalog_name(self) -> &'static str {
        match self {
            Self::Nvidia => "nvidia",
            Self::Apple => "apple",
        }
    }
}

/// Parsed NVIDIA driver version suitable for deterministic compatibility comparison.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NvidiaDriverVersion {
    components: Vec<u32>,
}

impl NvidiaDriverVersion {
    /// Returns whether this driver satisfies an artifact's minimum driver major version.
    pub fn satisfies(&self, minimum: NvidiaDriverMajorVersion) -> bool {
        self.components[0] >= minimum.get()
    }

    /// Returns the driver major version.
    pub fn major(&self) -> u32 {
        self.components[0]
    }
}

impl FromStr for NvidiaDriverVersion {
    type Err = NvidiaDriverVersionError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(NvidiaDriverVersionError::Malformed {
                observed: value.to_owned(),
            });
        }
        let components = value
            .split('.')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| NvidiaDriverVersionError::Malformed {
                observed: value.to_owned(),
            })?;
        if components.is_empty() {
            return Err(NvidiaDriverVersionError::Malformed {
                observed: value.to_owned(),
            });
        }
        Ok(Self { components })
    }
}

/// Non-zero minimum NVIDIA driver major version from an engine artifact contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NvidiaDriverMajorVersion(NonZeroU32);

impl NvidiaDriverMajorVersion {
    /// Validates a minimum NVIDIA driver major version.
    pub fn new(value: u32) -> Result<Self, NvidiaDriverVersionError> {
        NonZeroU32::new(value)
            .map(Self)
            .ok_or(NvidiaDriverVersionError::ZeroMinimum)
    }

    /// Returns the validated major version.
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// An NVIDIA driver fact or compatibility threshold was malformed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum NvidiaDriverVersionError {
    #[error(
        "NVIDIA driver version {observed:?} is malformed; expected dot-separated decimal numbers"
    )]
    Malformed { observed: String },

    #[error("minimum NVIDIA driver major version must be greater than zero")]
    ZeroMinimum,
}

/// Whether a node's accelerators own dedicated memory or allocate from its host memory pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryArchitecture {
    /// Accelerators own a dedicated memory pool separate from host memory.
    Dedicated,

    /// Accelerators allocate from the host memory pool, as on NVIDIA GB10 and Apple Silicon.
    Unified,
}

impl MemoryArchitecture {
    /// Converts a protocol value, rejecting the unspecified value.
    pub fn from_proto(value: AcceleratorMemoryArchitecture) -> Option<Self> {
        match value {
            AcceleratorMemoryArchitecture::Dedicated => Some(Self::Dedicated),
            AcceleratorMemoryArchitecture::Unified => Some(Self::Unified),
            AcceleratorMemoryArchitecture::Unspecified => None,
        }
    }

    /// Returns the protocol value for this architecture.
    pub fn to_proto(self) -> AcceleratorMemoryArchitecture {
        match self {
            Self::Dedicated => AcceleratorMemoryArchitecture::Dedicated,
            Self::Unified => AcceleratorMemoryArchitecture::Unified,
        }
    }
}

/// How a container runtime selects the accelerator devices of a workload: CDI device names such as
/// `nvidia.com/gpu=0`, or NVIDIA device indexes such as `0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceleratorDeviceSelector {
    Cdi,

    NvidiaIndex,
}

impl AcceleratorDeviceSelector {
    /// Validates a Docker execution target's declared accelerator transport and the device
    /// identifiers it selects, returning the selector the transport uses.
    pub fn for_docker_transport(
        transport: i32,
        device_ids: &[String],
    ) -> Result<Self, AcceleratorSelectionError> {
        let selector = match DockerAcceleratorTransport::try_from(transport) {
            Ok(DockerAcceleratorTransport::Cdi) => Self::Cdi,
            Ok(DockerAcceleratorTransport::LegacyNvidia) => Self::NvidiaIndex,
            Ok(DockerAcceleratorTransport::Unspecified) | Err(_) => {
                return Err(AcceleratorSelectionError::InvalidTransport);
            }
        };
        if device_ids.iter().any(|id| !selector.valid_device_id(id)) {
            return Err(AcceleratorSelectionError::InvalidIdentifiers);
        }
        Ok(selector)
    }

    /// Returns whether `value` is a device identifier in this selector's grammar.
    ///
    /// CDI validation accepts any qualified device name, a superset of what
    /// [`ResolvedAccelerators::resolve`] maps to observed devices; the rest resolve as unresolved.
    pub fn valid_device_id(self, value: &str) -> bool {
        match self {
            Self::Cdi => valid_cdi_device_id(value),
            Self::NvidiaIndex => decimal_index(value).is_some(),
        }
    }
}

/// Invalid accelerator selection declared for an execution target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AcceleratorSelectionError {
    #[error("requires CDI or Legacy NVIDIA transport")]
    InvalidTransport,

    #[error("has accelerator IDs invalid for its selected transport")]
    InvalidIdentifiers,
}

/// Node-scoped id of the one GPU every Apple Silicon Mac has, used both as its hardware device id
/// and as the accelerator selector of native darwin targets. Execution-target device lists in site
/// configuration outlive reboots, which IORegistry entry ids are not guaranteed to survive.
pub const APPLE_GPU_DEVICE_ID: &str = "apple-gpu-0";

/// Hardware device ids a target's accelerator selectors name in one node's hardware snapshot.
///
/// Execution targets declare runtime selectors (`nvidia.com/gpu=0`, `0`, `apple-gpu-0`) while
/// hardware facts carry vendor-native ids (`GPU-<uuid>`). This joins the two; it is a derived
/// observation, never intent or a reservation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedAccelerators {
    /// Deduplicated vendor-native device ids in snapshot order; empty unless the status is resolved.
    pub ids: Vec<String>,

    pub status: AcceleratorResolution,
}

impl ResolvedAccelerators {
    /// Resolves every declared selector against a node's latest hardware snapshot, all or nothing.
    ///
    /// NVIDIA index selectors (`nvidia.com/gpu=N`, `nvidia.com/gpu=gpuN`, legacy `N`) match the
    /// device whose NVML enumeration index is `N`, assuming the NVIDIA container toolkit numbers CDI
    /// devices in NVML order; a fact without an index never matches one, and an index two facts
    /// share stays unresolved. UUID selectors (`nvidia.com/gpu=GPU-<uuid>`, or the bare
    /// `GPU-<uuid>` device id that simulated targets name) match the device id,
    /// `nvidia.com/gpu=all` every NVIDIA device, and `apple-gpu-0` the Apple GPU. MIG and other
    /// selectors stay unresolved.
    pub fn resolve(selectors: &[String], snapshot: Option<&HardwareSnapshot>) -> Self {
        if selectors.is_empty() {
            return Self::without_ids(AcceleratorResolution::NotApplicable);
        }
        let Some(snapshot) = snapshot else {
            return Self::without_ids(AcceleratorResolution::SnapshotUnavailable);
        };

        let mut matched_ids = BTreeSet::new();
        let mut unreadable = false;
        let mut unmatched = false;
        for selector in selectors {
            let selected = SelectedAcceleratorDevice::parse(selector);
            let Some(vendor) = selected.vendor() else {
                return Self::without_ids(AcceleratorResolution::Unresolved);
            };
            let Some(facts) = vendor_facts(snapshot, vendor) else {
                unreadable = true;
                continue;
            };
            let matches = facts
                .iter()
                .filter(|fact| selected.matches(*fact))
                .collect::<Vec<_>>();
            // A selector naming one device must match exactly one, so a duplicate index never guesses.
            let matched = match selected {
                SelectedAcceleratorDevice::NvidiaAll => !matches.is_empty(),
                _ => matches.len() == 1,
            };
            unmatched |= !matched;
            matched_ids.extend(matches.iter().map(|fact| fact.device_id.as_str()));
        }

        if unreadable {
            Self::without_ids(AcceleratorResolution::SnapshotUnavailable)
        } else if unmatched {
            Self::without_ids(AcceleratorResolution::Unresolved)
        } else {
            let mut ids = Vec::with_capacity(matched_ids.len());
            for fact in snapshot_facts(snapshot) {
                if matched_ids.remove(fact.device_id.as_str()) {
                    ids.push(fact.device_id.clone());
                }
            }
            Self {
                ids,
                status: AcceleratorResolution::Resolved,
            }
        }
    }

    /// Returns the single resolved device id, or none when resolution failed or named several.
    pub fn single_id(&self) -> Option<&str> {
        match self.ids.as_slice() {
            [id] => Some(id),
            _ => None,
        }
    }

    fn without_ids(status: AcceleratorResolution) -> Self {
        Self {
            ids: Vec::new(),
            status,
        }
    }
}

/// Returns the devices a target's accelerator selectors name among one vendor's `facts`,
/// deduplicated in fact order, with the same matching rules as [`ResolvedAccelerators::resolve`].
/// Returns none when any selector names no single device.
pub fn selected_accelerator_facts<'a, D: SelectableAcceleratorDevice>(
    selectors: &[String],
    facts: &'a [D],
) -> Option<Vec<&'a D>> {
    let mut selected = vec![false; facts.len()];
    for selector in selectors {
        let device = SelectedAcceleratorDevice::parse(selector);
        let matches = (0..facts.len())
            .filter(|index| device.matches(&facts[*index]))
            .collect::<Vec<_>>();
        let matched = match device {
            SelectedAcceleratorDevice::NvidiaAll => !matches.is_empty(),
            _ => matches.len() == 1,
        };
        if !matched {
            return None;
        }
        for index in matches {
            selected[index] = true;
        }
    }

    Some(
        facts
            .iter()
            .zip(selected)
            .filter_map(|(fact, selected)| selected.then_some(fact))
            .collect(),
    )
}

/// An observed accelerator device as selectors name it: by its vendor-native id or by its vendor
/// enumeration index.
pub trait SelectableAcceleratorDevice {
    /// Vendor-native immutable identity of the device.
    fn device_id(&self) -> &str;

    /// Enumeration index the vendor gives the device, absent when unreported.
    fn vendor_index(&self) -> Option<u32>;
}

impl SelectableAcceleratorDevice for AcceleratorFacts {
    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn vendor_index(&self) -> Option<u32> {
        self.vendor_index
    }
}

/// Outcome of resolving a target's accelerator selectors to observed hardware device ids.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcceleratorResolution {
    /// Every selector matched at least one observed device.
    Resolved,

    /// At least one selector matched no observed device or has an unsupported form, such as MIG.
    Unresolved,

    /// The node has no hardware snapshot, or the vendor facts a selector needs are unreadable.
    SnapshotUnavailable,

    /// The target declares no selectors, such as a simulated target.
    NotApplicable,
}

impl AcceleratorResolution {
    /// Returns the protocol value for this outcome.
    pub fn to_proto(self) -> AcceleratorResolutionStatus {
        match self {
            Self::Resolved => AcceleratorResolutionStatus::Resolved,
            Self::Unresolved => AcceleratorResolutionStatus::Unresolved,
            Self::SnapshotUnavailable => AcceleratorResolutionStatus::SnapshotUnavailable,
            Self::NotApplicable => AcceleratorResolutionStatus::NotApplicable,
        }
    }
}

/// Returns the canonical NVIDIA CDI selector of one full GPU, such as `nvidia.com/gpu=gpu0`.
pub fn nvidia_cdi_gpu_selector(index: u32) -> String {
    format!("{NVIDIA_CDI_GPU_PREFIX}gpu{index}")
}

/// Returns the older bare-index NVIDIA CDI selector of one full GPU, such as `nvidia.com/gpu=0`.
pub fn nvidia_cdi_legacy_gpu_selector(index: u32) -> String {
    format!("{NVIDIA_CDI_GPU_PREFIX}{index}")
}

/// Returns the device index a full-GPU NVIDIA CDI selector names: `nvidia.com/gpu=N` or
/// `nvidia.com/gpu=gpuN`. Other CDI devices, such as `all`, UUIDs, or MIG slices, name none.
pub fn nvidia_cdi_gpu_index(selector: &str) -> Option<u32> {
    let device = selector.strip_prefix(NVIDIA_CDI_GPU_PREFIX)?;
    decimal_index(device.strip_prefix("gpu").unwrap_or(device))
}

/// Returns whether a selector is the CDI device name of every device of its class, such as
/// `nvidia.com/gpu=all`. Model execution targets name individual devices, so they reject it.
pub fn names_every_cdi_device(selector: &str) -> bool {
    selector
        .split_once('=')
        .is_some_and(|(kind, device)| kind.contains('/') && device == "all")
}

const NVIDIA_CDI_GPU_PREFIX: &str = "nvidia.com/gpu=";

/// Device or devices one declared accelerator selector names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectedAcceleratorDevice<'a> {
    /// NVIDIA GPU by enumeration index, from a CDI or legacy bare-index selector.
    NvidiaIndex(u32),

    /// NVIDIA GPU by its UUID device id.
    NvidiaUuid(&'a str),

    NvidiaAll,

    AppleGpu,

    /// A form no hardware fact can be matched to, such as a MIG slice.
    Unsupported,
}

impl<'a> SelectedAcceleratorDevice<'a> {
    fn parse(selector: &'a str) -> Self {
        if selector == APPLE_GPU_DEVICE_ID {
            return Self::AppleGpu;
        }
        if let Some(index) = decimal_index(selector).or_else(|| nvidia_cdi_gpu_index(selector)) {
            return Self::NvidiaIndex(index);
        }
        match selector.strip_prefix(NVIDIA_CDI_GPU_PREFIX) {
            Some("all") => Self::NvidiaAll,
            Some(uuid) if uuid.starts_with("GPU-") => Self::NvidiaUuid(uuid),
            // Simulated targets name their declared machine's GPUs by device id.
            None if selector.starts_with("GPU-") => Self::NvidiaUuid(selector),
            _ => Self::Unsupported,
        }
    }

    fn vendor(self) -> Option<AcceleratorVendor> {
        match self {
            Self::NvidiaIndex(_) | Self::NvidiaUuid(_) | Self::NvidiaAll => {
                Some(AcceleratorVendor::Nvidia)
            }
            Self::AppleGpu => Some(AcceleratorVendor::Apple),
            Self::Unsupported => None,
        }
    }

    fn matches(self, device: &impl SelectableAcceleratorDevice) -> bool {
        match self {
            Self::NvidiaIndex(index) => device.vendor_index() == Some(index),
            Self::NvidiaUuid(uuid) => device.device_id() == uuid,
            Self::NvidiaAll => true,
            Self::AppleGpu => device.device_id() == APPLE_GPU_DEVICE_ID,
            Self::Unsupported => false,
        }
    }
}

/// Returns one vendor's device facts, empty when the vendor reported none, or none when its facts
/// are unreadable.
fn vendor_facts(
    snapshot: &HardwareSnapshot,
    vendor: AcceleratorVendor,
) -> Option<&[AcceleratorFacts]> {
    let Some(observation) = snapshot
        .accelerators
        .iter()
        .find(|observation| observation.vendor == vendor as i32)
    else {
        return Some(&[]);
    };
    match observation.result.as_ref()? {
        accelerator_observation::Result::Facts(set) => Some(&set.facts),
        accelerator_observation::Result::Unavailable(_) => None,
    }
}

/// Returns every readable device fact of the snapshot in its reported order.
fn snapshot_facts(snapshot: &HardwareSnapshot) -> impl Iterator<Item = &AcceleratorFacts> {
    snapshot
        .accelerators
        .iter()
        .filter_map(|observation| match observation.result.as_ref()? {
            accelerator_observation::Result::Facts(set) => Some(&set.facts),
            accelerator_observation::Result::Unavailable(_) => None,
        })
        .flatten()
}

/// Parses a plain decimal device index, rejecting signs, spaces and empty values.
fn decimal_index(value: &str) -> Option<u32> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Returns whether `value` is a qualified CDI device name: `vendor/class=device`, with a dotted
/// vendor domain.
fn valid_cdi_device_id(value: &str) -> bool {
    let Some((vendor, qualified_device)) = value.split_once('/') else {
        return false;
    };
    let Some((class, device)) = qualified_device.split_once('=') else {
        return false;
    };
    !vendor.is_empty()
        && vendor.contains('.')
        && !class.is_empty()
        && !device.is_empty()
        && [vendor, class, device].iter().all(|part| {
            part.len() <= 128
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ':')
                })
        })
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use dotcore_proto::dotcore::observation::v1::{
        AcceleratorFacts, AcceleratorFactsSet, AcceleratorObservation, AcceleratorVendor,
        HardwareSnapshot, ObservationUnavailable, accelerator_observation,
    };

    use super::{
        AcceleratorResolution, NvidiaDriverMajorVersion, NvidiaDriverVersion, ResolvedAccelerators,
        names_every_cdi_device, nvidia_cdi_gpu_index, selected_accelerator_facts,
    };

    const FIRST_GPU: &str = "GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b";
    const SECOND_GPU: &str = "GPU-3c1e7b52-8d0a-4f6e-b1a9-52d7e0c4a816";

    #[test]
    fn nvidia_driver_versions_use_typed_major_thresholds() {
        let minimum = NvidiaDriverMajorVersion::new(580).expect("minimum");

        assert!(
            !NvidiaDriverVersion::from_str("579.999")
                .expect("579")
                .satisfies(minimum)
        );
        assert!(
            NvidiaDriverVersion::from_str("580")
                .expect("580")
                .satisfies(minimum)
        );
        assert!(
            NvidiaDriverVersion::from_str("580.173.02")
                .expect("580.173.02")
                .satisfies(minimum)
        );
        assert!(
            NvidiaDriverVersion::from_str("581.1")
                .expect("581")
                .satisfies(minimum)
        );
        for malformed in ["", "driver-580", "580.beta", "580..1", ".580"] {
            assert!(
                NvidiaDriverVersion::from_str(malformed).is_err(),
                "accepted {malformed:?}"
            );
        }
    }

    #[test]
    fn selectors_resolve_to_observed_device_ids() {
        let two_gpus = nvidia_snapshot(vec![
            nvidia(FIRST_GPU, Some(0)),
            nvidia(SECOND_GPU, Some(1)),
        ]);
        let resolved = |ids: &[&str]| ResolvedAccelerators {
            ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            status: AcceleratorResolution::Resolved,
        };
        let failed = |status| ResolvedAccelerators {
            ids: Vec::new(),
            status,
        };
        let cases = [
            (
                "cdi index",
                vec!["nvidia.com/gpu=0"],
                resolved(&[FIRST_GPU]),
            ),
            (
                "cdi gpu name",
                vec!["nvidia.com/gpu=gpu1"],
                resolved(&[SECOND_GPU]),
            ),
            (
                "cdi uuid",
                vec!["nvidia.com/gpu=GPU-3c1e7b52-8d0a-4f6e-b1a9-52d7e0c4a816"],
                resolved(&[SECOND_GPU]),
            ),
            (
                "cdi all",
                vec!["nvidia.com/gpu=all"],
                resolved(&[FIRST_GPU, SECOND_GPU]),
            ),
            ("legacy index", vec!["1"], resolved(&[SECOND_GPU])),
            (
                "multi selector",
                vec!["nvidia.com/gpu=gpu1", "nvidia.com/gpu=0"],
                resolved(&[FIRST_GPU, SECOND_GPU]),
            ),
            (
                "overlapping selectors",
                vec!["nvidia.com/gpu=all", "nvidia.com/gpu=1"],
                resolved(&[FIRST_GPU, SECOND_GPU]),
            ),
            (
                "mig slice",
                vec!["nvidia.com/gpu=0:1"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "mig uuid",
                vec!["nvidia.com/gpu=MIG-0b0a4f9e-12c4-5d6f-8e9a-1b2c3d4e5f60"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "out-of-range index",
                vec!["nvidia.com/gpu=2"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "unknown uuid",
                vec!["nvidia.com/gpu=GPU-00000000-0000-0000-0000-000000000000"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "one unresolved selector",
                vec!["nvidia.com/gpu=0", "nvidia.com/gpu=5"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "apple on nvidia node",
                vec!["apple-gpu-0"],
                failed(AcceleratorResolution::Unresolved),
            ),
            (
                "simulated device id",
                vec!["GPU-3c1e7b52-8d0a-4f6e-b1a9-52d7e0c4a816"],
                resolved(&[SECOND_GPU]),
            ),
            (
                "no selectors",
                vec![],
                failed(AcceleratorResolution::NotApplicable),
            ),
        ];
        for (name, selectors, expected) in cases {
            let selectors = selectors.into_iter().map(str::to_owned).collect::<Vec<_>>();

            assert_eq!(
                ResolvedAccelerators::resolve(&selectors, Some(&two_gpus)),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn selectors_never_match_by_fact_position() {
        let without_indexes =
            nvidia_snapshot(vec![nvidia(FIRST_GPU, None), nvidia(SECOND_GPU, None)]);
        let reordered = nvidia_snapshot(vec![
            nvidia(SECOND_GPU, Some(1)),
            nvidia(FIRST_GPU, Some(0)),
        ]);
        let duplicate_indexes = nvidia_snapshot(vec![
            nvidia(FIRST_GPU, Some(0)),
            nvidia(SECOND_GPU, Some(0)),
        ]);
        let selectors = |values: &[&str]| {
            values
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>()
        };
        let unresolved = ResolvedAccelerators {
            ids: Vec::new(),
            status: AcceleratorResolution::Unresolved,
        };

        assert_eq!(
            ResolvedAccelerators::resolve(
                &selectors(&["nvidia.com/gpu=0"]),
                Some(&without_indexes)
            ),
            unresolved
        );
        assert_eq!(
            ResolvedAccelerators::resolve(
                &selectors(&["nvidia.com/gpu=0"]),
                Some(&duplicate_indexes)
            ),
            unresolved
        );
        assert_eq!(
            ResolvedAccelerators::resolve(
                &selectors(&["nvidia.com/gpu=GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b"]),
                Some(&without_indexes)
            )
            .ids,
            [FIRST_GPU]
        );
        assert_eq!(
            ResolvedAccelerators::resolve(&selectors(&["nvidia.com/gpu=0"]), Some(&reordered)).ids,
            [FIRST_GPU]
        );
    }

    #[test]
    fn apple_gpu_resolves_by_its_device_identity() {
        let apple = HardwareSnapshot {
            observed_at: None,
            host: None,
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Apple as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet {
                        facts: vec![AcceleratorFacts {
                            vendor: AcceleratorVendor::Apple as i32,
                            device_id: "apple-gpu-0".to_owned(),
                            vendor_index: Some(0),
                            ..AcceleratorFacts::default()
                        }],
                    },
                )),
            }],
        };

        assert_eq!(
            ResolvedAccelerators::resolve(&["apple-gpu-0".to_owned()], Some(&apple)),
            ResolvedAccelerators {
                ids: vec!["apple-gpu-0".to_owned()],
                status: AcceleratorResolution::Resolved,
            }
        );
    }

    #[test]
    fn missing_or_unreadable_snapshots_are_unavailable() {
        let selectors = vec!["nvidia.com/gpu=0".to_owned()];
        let unreadable = HardwareSnapshot {
            observed_at: None,
            host: None,
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Nvidia as i32,
                result: Some(accelerator_observation::Result::Unavailable(
                    ObservationUnavailable {
                        code: "COLLECTION_FAILED".to_owned(),
                        message: "NVML is unavailable.".to_owned(),
                    },
                )),
            }],
        };
        let unavailable = ResolvedAccelerators {
            ids: Vec::new(),
            status: AcceleratorResolution::SnapshotUnavailable,
        };

        assert_eq!(ResolvedAccelerators::resolve(&selectors, None), unavailable);
        assert_eq!(
            ResolvedAccelerators::resolve(&selectors, Some(&unreadable)),
            unavailable
        );
        assert_eq!(
            ResolvedAccelerators::resolve(&[], None).status,
            AcceleratorResolution::NotApplicable
        );
    }

    #[test]
    fn nvidia_cdi_gpu_index_reads_only_full_gpu_selectors() {
        for (selector, expected) in [
            ("nvidia.com/gpu=0", Some(0)),
            ("nvidia.com/gpu=gpu12", Some(12)),
            ("nvidia.com/gpu=all", None),
            (
                "nvidia.com/gpu=GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b",
                None,
            ),
            ("nvidia.com/gpu=0:1", None),
            ("nvidia.com/gpu=+1", None),
            ("nvidia.com/gpu=", None),
            ("1", None),
        ] {
            assert_eq!(nvidia_cdi_gpu_index(selector), expected, "{selector}");
        }
    }

    #[test]
    fn only_the_all_cdi_device_names_every_device() {
        for (selector, expected) in [
            ("nvidia.com/gpu=all", true),
            ("vendor.example/class=all", true),
            ("nvidia.com/gpu=gpu0", false),
            ("nvidia.com/gpu=0", false),
            (
                "nvidia.com/gpu=GPU-f9a4a3aa-f02d-97ca-9972-3f61413b228b",
                false,
            ),
            ("nvidia.com/gpu=0:1", false),
            ("all", false),
            ("0", false),
        ] {
            assert_eq!(names_every_cdi_device(selector), expected, "{selector}");
        }
    }

    #[test]
    fn selected_accelerator_facts_names_selected_devices_in_fact_order() {
        let facts = vec![
            nvidia("GPU-a", Some(0)),
            nvidia("GPU-b", Some(1)),
            nvidia("GPU-c", Some(2)),
        ];
        let device_ids = |selectors: &[&str]| {
            let selectors = selectors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>();
            selected_accelerator_facts(&selectors, &facts).map(|selected| {
                selected
                    .iter()
                    .map(|fact| fact.device_id.as_str())
                    .collect::<Vec<_>>()
            })
        };

        assert_eq!(
            device_ids(&["nvidia.com/gpu=gpu2", "nvidia.com/gpu=0"]),
            Some(vec!["GPU-a", "GPU-c"])
        );
        assert_eq!(
            device_ids(&["nvidia.com/gpu=GPU-b", "nvidia.com/gpu=1"]),
            Some(vec!["GPU-b"])
        );
        assert_eq!(
            device_ids(&["nvidia.com/gpu=all"]),
            Some(vec!["GPU-a", "GPU-b", "GPU-c"])
        );
        assert_eq!(device_ids(&["nvidia.com/gpu=gpu3"]), None);
        assert_eq!(device_ids(&["nvidia.com/gpu=0:1"]), None);
    }

    fn nvidia_snapshot(facts: Vec<AcceleratorFacts>) -> HardwareSnapshot {
        HardwareSnapshot {
            observed_at: None,
            host: None,
            accelerators: vec![AcceleratorObservation {
                vendor: AcceleratorVendor::Nvidia as i32,
                result: Some(accelerator_observation::Result::Facts(
                    AcceleratorFactsSet { facts },
                )),
            }],
        }
    }

    fn nvidia(device_id: &str, vendor_index: Option<u32>) -> AcceleratorFacts {
        AcceleratorFacts {
            vendor: AcceleratorVendor::Nvidia as i32,
            model: "NVIDIA GeForce RTX 5090".to_owned(),
            device_id: device_id.to_owned(),
            vendor_index,
            total_memory_bytes: Some(34_190_917_632),
            ..AcceleratorFacts::default()
        }
    }
}
