//! Confirms one execution target's current capabilities from its fresh reconciled observations
//! and selects the most specific supported hardware class that describes them.
//!
//! Every reader of a target's capabilities goes through this one rule, so a platform, accelerator
//! vendor, memory architecture and device model mean the same thing wherever they are read. A
//! missing, stale, or mismatched observation is a typed failure, never a guess; only advisory
//! reads about machines that stopped reporting may accept the last observations instead. The
//! selected class is unique because catalog validation keeps supported classes disjoint or nested.

use std::time::SystemTime;

use dotcore_catalog::{SelectorRelation, SupportedHardwareClass, selector_relation};
use dotcore_domain::{
    Accelerator, ExecutionPlatform, MemoryArchitecture, NvidiaDriverVersion, WorkloadArtifactKind,
};
use dotcore_proto::dotcore::{catalog::v1::Selector, site::v1::ExecutionTarget};
use dotcore_site::node::target_inventory::{
    CapabilityObservation, NodeMemoryFacts, TargetCapabilityInventory,
};

use crate::hardware::capacity::{NodeHardware, TargetCapacity};

/// Fresh capability values observed for one exact configured execution target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectedTargetCapabilities {
    pub(crate) platform: ExecutionPlatform,
    pub(crate) accelerator: TargetAccelerator,
    /// Artifact kinds the target's configured runtime binding can realize.
    pub(crate) artifact_kinds: &'static [WorkloadArtifactKind],
    pub(crate) memory_architecture: MemoryArchitecture,
    /// Product names of the devices the target uses, as reported; empty for a device that reported
    /// none. Empty when the node reported no devices.
    pub(crate) device_models: Vec<String>,
    /// Memory pools the target draws from, built by the capacity component from the same
    /// observation. Their measured use is transient and never reaches a deployment snapshot.
    pub(crate) capacity: TargetCapacity,
}

impl SelectedTargetCapabilities {
    /// Total of the pool the target's accelerator memory comes from, absent when it could not be
    /// determined: the device total on dedicated memory, the smallest one when the target holds
    /// several devices, or the host total on unified memory. Engines divide by it; it is never a
    /// capacity reservation.
    pub(crate) fn memory_pool_total_bytes(&self) -> Option<u64> {
        self.capacity.accelerator_total_bytes()
    }
}

/// Fresh accelerator vendor of one target, with the facts only that vendor needs for selection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TargetAccelerator {
    /// NVIDIA accelerators and their shared driver version.
    Nvidia { driver: NvidiaDriverVersion },

    /// Apple Silicon GPU, which has no driver minimum.
    Apple,
}

impl TargetAccelerator {
    /// Returns the vendor that selectors name.
    pub(crate) fn vendor(&self) -> Accelerator {
        match self {
            Self::Nvidia { .. } => Accelerator::Nvidia,
            Self::Apple => Accelerator::Apple,
        }
    }
}

/// Validates an execution target against its capability observation, judging freshness at `now`,
/// the backend clock's wall time.
pub(crate) fn select_target_capabilities(
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
    now: SystemTime,
) -> Result<SelectedTargetCapabilities, TargetCapabilitySelectionError> {
    select_observed_capabilities(target, inventory, Some(now))
}

/// Validates an execution target against its last capability observation, however old. Only
/// advisory reads about machines that stopped reporting use it; measured memory use is dropped
/// from stale memory facts, so only totals count.
pub(crate) fn select_last_observed_target_capabilities(
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
) -> Result<SelectedTargetCapabilities, TargetCapabilitySelectionError> {
    select_observed_capabilities(target, inventory, None)
}

/// Validates an execution target against its capability observation, requiring the platform and
/// accelerator observations to be fresh at `fresh_at` when given.
fn select_observed_capabilities(
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
    fresh_at: Option<SystemTime>,
) -> Result<SelectedTargetCapabilities, TargetCapabilitySelectionError> {
    let accepted = |observed_at| fresh_at.is_none_or(|now| inventory.is_fresh(observed_at, now));
    let pinned = target
        .platform
        .as_ref()
        .ok_or(TargetCapabilitySelectionError::MissingTargetPlatform)
        .and_then(|platform| {
            ExecutionPlatform::try_from(platform)
                .map_err(TargetCapabilitySelectionError::InvalidTargetPlatform)
        })?;
    let capabilities = inventory
        .for_target(&target.execution_target_id)
        .ok_or_else(|| TargetCapabilitySelectionError::MissingObservation {
            execution_target_id: target.execution_target_id.clone(),
        })?;
    let observed_platform = match &capabilities.platform {
        CapabilityObservation::Available(platform)
            if accepted(capabilities.platform_observed_at) =>
        {
            platform
        }
        CapabilityObservation::Stale(platform) if fresh_at.is_none() => platform,
        CapabilityObservation::Available(_) | CapabilityObservation::Stale(_) => {
            return Err(TargetCapabilitySelectionError::StalePlatform {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
        CapabilityObservation::Unavailable => {
            return Err(TargetCapabilitySelectionError::PlatformUnavailable {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
    };
    if observed_platform != &pinned {
        return Err(TargetCapabilitySelectionError::PlatformMismatch {
            execution_target_id: target.execution_target_id.clone(),
            observed_operating_system: observed_platform.operating_system().to_owned(),
            observed_architecture: observed_platform.architecture().to_owned(),
            required_operating_system: pinned.operating_system().to_owned(),
            required_architecture: pinned.architecture().to_owned(),
        });
    }
    let vendor = match capabilities.accelerator {
        CapabilityObservation::Available(vendor)
            if accepted(capabilities.accelerator_observed_at) =>
        {
            vendor
        }
        CapabilityObservation::Stale(vendor) if fresh_at.is_none() => vendor,
        CapabilityObservation::Available(_) | CapabilityObservation::Stale(_) => {
            return Err(TargetCapabilitySelectionError::StaleAccelerator {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
        CapabilityObservation::Unavailable => {
            return Err(TargetCapabilitySelectionError::AcceleratorUnavailable {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
    };
    // The driver shares the accelerator observation, so its freshness is already confirmed.
    let accelerator = match (vendor, &capabilities.nvidia_driver) {
        (
            Accelerator::Nvidia,
            CapabilityObservation::Available(driver) | CapabilityObservation::Stale(driver),
        ) => TargetAccelerator::Nvidia {
            driver: driver.clone(),
        },
        (Accelerator::Nvidia, CapabilityObservation::Unavailable) => {
            return Err(TargetCapabilitySelectionError::NvidiaDriverUnavailable {
                execution_target_id: target.execution_target_id.clone(),
            });
        }
        (Accelerator::Apple, _) => TargetAccelerator::Apple,
    };
    let memory_architecture = match capabilities.accelerator_memory {
        CapabilityObservation::Available(architecture)
        | CapabilityObservation::Stale(architecture) => architecture,
        CapabilityObservation::Unavailable => {
            return Err(
                TargetCapabilitySelectionError::AcceleratorMemoryUnavailable {
                    execution_target_id: target.execution_target_id.clone(),
                },
            );
        }
    };
    let device_models = target_device_models(
        &capabilities.memory,
        capabilities.accelerator_device_ids.as_deref(),
    );
    // Unknown memory never fails selection; only engines that divide by the total require it.
    let capacity = TargetCapacity::for_target(
        memory_architecture,
        &NodeHardware::observed(&capabilities.memory),
        capabilities.accelerator_device_ids.as_deref(),
    );

    Ok(SelectedTargetCapabilities {
        platform: pinned,
        accelerator,
        artifact_kinds: WorkloadArtifactKind::realizable_by(target),
        memory_architecture,
        device_models,
        capacity,
    })
}

/// Product names of the devices a target uses: the devices its accelerator selectors name, or
/// every device of its node when they name none, which covers whichever devices it uses.
fn target_device_models(
    memory: &CapabilityObservation<NodeMemoryFacts>,
    device_ids: Option<&[String]>,
) -> Vec<String> {
    let (CapabilityObservation::Available(facts) | CapabilityObservation::Stale(facts)) = memory
    else {
        return Vec::new();
    };
    facts
        .devices
        .iter()
        .filter(|device| device_ids.is_none_or(|ids| ids.contains(&device.device_id)))
        .map(|device| device.model.clone())
        .collect()
}

/// Failures while confirming one execution target's current capabilities.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TargetCapabilitySelectionError {
    #[error("execution target has no pinned platform")]
    MissingTargetPlatform,

    #[error("execution target {execution_target_id:?} has no capability observation")]
    MissingObservation { execution_target_id: String },

    #[error("execution target {execution_target_id:?} has a stale platform observation")]
    StalePlatform { execution_target_id: String },

    #[error("execution target {execution_target_id:?} has no available platform observation")]
    PlatformUnavailable { execution_target_id: String },

    #[error(
        "execution target {execution_target_id:?} reports {observed_operating_system}/{observed_architecture}, but is configured for {required_operating_system}/{required_architecture}"
    )]
    PlatformMismatch {
        execution_target_id: String,
        observed_operating_system: String,
        observed_architecture: String,
        required_operating_system: String,
        required_architecture: String,
    },

    #[error("execution target {execution_target_id:?} has a stale accelerator observation")]
    StaleAccelerator { execution_target_id: String },

    #[error(
        "execution target {execution_target_id:?} has no available accelerator observation; its node reports no NVIDIA or Apple accelerator"
    )]
    AcceleratorUnavailable { execution_target_id: String },

    #[error(
        "execution target {execution_target_id:?} reports NVIDIA accelerators without one valid shared driver version"
    )]
    NvidiaDriverUnavailable { execution_target_id: String },

    #[error(
        "execution target {execution_target_id:?} has no classified accelerator memory architecture; its accelerator devices disagree or report none"
    )]
    AcceleratorMemoryUnavailable { execution_target_id: String },

    #[error("execution target platform is missing or unsupported: {0}")]
    InvalidTargetPlatform(#[source] dotcore_domain::ExecutionPlatformError),
}

/// Selects the most specific supported hardware class whose selector matches the target's
/// platform, accelerator vendor, memory architecture and device models. Catalog validation keeps one owner's
/// supported classes disjoint or strictly nested, so the most specific match is unique.
pub(crate) fn select_hardware_class(
    supported: Vec<SupportedHardwareClass>,
    target: &SelectedTargetCapabilities,
) -> Result<SupportedHardwareClass, HardwareClassSelectionError> {
    let platform = &target.platform;
    let vendor = target.accelerator.vendor();
    let matching = supported
        .into_iter()
        .filter_map(|support| {
            let selector = support.hardware_class.selector.clone()?;
            selector_matches(&selector, target).then_some((support, selector))
        })
        .collect::<Vec<_>>();
    let most_specific = matching.iter().find(|(candidate, selector)| {
        matching.iter().all(|(other, other_selector)| {
            other.hardware_class.hardware_class_id == candidate.hardware_class.hardware_class_id
                || selector_relation(selector, other_selector) == SelectorRelation::Narrower
        })
    });
    match most_specific {
        Some((support, _)) => Ok(support.clone()),
        None if matching.is_empty() => Err(HardwareClassSelectionError::NoMatch {
            operating_system: platform.operating_system().to_owned(),
            architecture: platform.architecture().to_owned(),
            accelerator: vendor.catalog_name(),
            memory_architecture: target.memory_architecture,
        }),
        None => Err(HardwareClassSelectionError::Ambiguous {
            hardware_class_ids: matching
                .iter()
                .map(|(support, _)| support.hardware_class.hardware_class_id.clone())
                .collect(),
        }),
    }
}

/// Failures while selecting a supported hardware class for one target.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum HardwareClassSelectionError {
    #[error(
        "no supported hardware class matches target platform {operating_system}/{architecture} with {accelerator} accelerators and {} memory",
        memory_architecture_name(*memory_architecture)
    )]
    NoMatch {
        operating_system: String,
        architecture: String,
        accelerator: &'static str,
        memory_architecture: MemoryArchitecture,
    },

    #[error(
        "supported hardware classes {hardware_class_ids:?} all match the target with none more specific"
    )]
    Ambiguous { hardware_class_ids: Vec<String> },
}

pub(crate) fn memory_architecture_name(architecture: MemoryArchitecture) -> &'static str {
    match architecture {
        MemoryArchitecture::Dedicated => "dedicated",
        MemoryArchitecture::Unified => "unified",
    }
}

/// Whether a selector matches the target's platform, accelerator vendor, memory architecture and
/// device models. A dimension the selector leaves unset matches any value; device models match only
/// when the target reports devices and every one reports a listed model. Selected targets never
/// carry a platform variant.
pub(crate) fn selector_matches(selector: &Selector, target: &SelectedTargetCapabilities) -> bool {
    let platform = &target.platform;
    let vendor = target.accelerator.vendor();
    let platform_matches = selector.platform.as_ref().is_none_or(|candidate| {
        candidate.operating_system == platform.operating_system()
            && candidate.architecture == platform.architecture()
            && candidate.variant.as_deref().is_none_or(str::is_empty)
    });
    let accelerator_matches = selector.accelerators.is_empty()
        || selector
            .accelerators
            .iter()
            .any(|name| name == vendor.catalog_name());
    let memory_architecture_matches =
        MemoryArchitecture::from_proto(selector.memory_architecture())
            .is_none_or(|architecture| architecture == target.memory_architecture);
    let device_models_match = selector.device_models.is_empty()
        || (!target.device_models.is_empty()
            && target
                .device_models
                .iter()
                .all(|model| selector.device_models.contains(model)));
    platform_matches && accelerator_matches && memory_architecture_matches && device_models_match
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        str::FromStr,
        time::{Duration, SystemTime},
    };

    use dotcore_catalog::{SupportedHardwareClass, test_support::vendor_hardware_class};
    use dotcore_domain::{
        ExecutionPlatform, MemoryArchitecture, NvidiaDriverVersion, WorkloadArtifactKind,
    };
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::{
        observation::v1::AcceleratorMemoryArchitecture,
        site::v1::{
            ExecutionTarget, ModelRuntimeBinding, execution_target::RuntimeBinding,
            model_runtime_binding,
        },
    };
    use dotcore_site::node::target_inventory::{
        NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
        TargetCapabilityInventory, TargetCapabilityReconciler,
    };

    use super::{
        HardwareClassSelectionError, SelectedTargetCapabilities, TargetAccelerator,
        TargetCapabilitySelectionError, select_hardware_class,
        select_last_observed_target_capabilities, select_target_capabilities,
    };
    use crate::hardware::capacity::TargetCapacity;

    const TARGET_CAPABILITY_MAX_AGE: std::time::Duration =
        dotcore_domain::timing::Timing::production()
            .node_control
            .capability_max_age;

    const OCI: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Oci];
    const NIX: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Nix];
    const ALL: &[WorkloadArtifactKind] = WorkloadArtifactKind::ALL;

    #[test]
    fn hardware_class_selection_picks_the_matching_class_for_each_target() {
        let supported = [
            supported_class("nvidia", None, &["nvidia"]),
            supported_class("apple", None, &["apple"]),
        ];
        let gb10 = nvidia_target(&ExecutionPlatform::linux_arm64(), "580.95.05");
        let rtx = nvidia_target(&ExecutionPlatform::linux_amd64(), "580.95.05");

        assert_eq!(
            selected_class_id(&supported, &gb10),
            Ok("nvidia".to_owned())
        );
        assert_eq!(selected_class_id(&supported, &rtx), Ok("nvidia".to_owned()));
        assert_eq!(
            selected_class_id(&supported, &apple_target()),
            Ok("apple".to_owned())
        );
    }

    #[test]
    fn observed_gb10_rtx_mac_native_and_docker_desktop_facts_select_their_hardware_class() {
        let supported = [
            memory_class("nvidia-unified", AcceleratorMemoryArchitecture::Unified),
            memory_class("nvidia-dedicated", AcceleratorMemoryArchitecture::Dedicated),
            supported_class("apple", None, &["apple"]),
        ];
        let amd64 = ExecutionPlatform::linux_amd64();
        let arm64 = ExecutionPlatform::linux_arm64();
        let darwin = ExecutionPlatform::darwin_arm64();
        let class_for = |target: ExecutionTarget, accelerator: NodeAcceleratorObservation| {
            let platform = ExecutionPlatform::try_from(target.platform.as_ref().expect("platform"))
                .expect("execution platform");
            let inventory = TargetCapabilityReconciler::reconcile(
                [("target-a", "node-a")],
                [RuntimePlatformObservation::available("target-a", platform)],
                [accelerator],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            );
            let capabilities =
                select_target_capabilities(&target, &inventory, std::time::SystemTime::now())?;
            Ok::<_, TargetCapabilitySelectionError>(selected_class_id(&supported, &capabilities))
        };
        let gb10 = || {
            NodeAcceleratorObservation::nvidia("node-a", "580")
                .with_memory_architecture(Some(MemoryArchitecture::Unified))
        };
        let rtx = || NodeAcceleratorObservation::nvidia("node-a", "580");

        // NVIDIA GB10 reports unified memory to Docker and native targets alike.
        assert_eq!(
            class_for(execution_target(&arm64, &["target-a"]), gb10()),
            Ok(Ok("nvidia-unified".to_owned()))
        );
        assert_eq!(
            class_for(native_target(&arm64), gb10()),
            Ok(Ok("nvidia-unified".to_owned()))
        );

        // An RTX card owns dedicated memory on Docker and native targets alike.
        assert_eq!(
            class_for(execution_target(&amd64, &["target-a"]), rtx()),
            Ok(Ok("nvidia-dedicated".to_owned()))
        );
        assert_eq!(
            class_for(native_target(&amd64), rtx()),
            Ok(Ok("nvidia-dedicated".to_owned()))
        );

        // macOS native runtime and Docker Desktop (linux/arm64) both see the Apple GPU.
        let apple =
            || NodeAcceleratorObservation::available("node-a", NodeAcceleratorFacts::apple());
        assert_eq!(
            class_for(native_target(&darwin), apple()),
            Ok(Ok("apple".to_owned()))
        );
        assert_eq!(
            class_for(execution_target(&arm64, &["target-a"]), apple()),
            Ok(Ok("apple".to_owned()))
        );

        // Docker Desktop reporting no accelerator cannot run: no target capabilities, no class.
        assert_eq!(
            class_for(
                execution_target(&arm64, &["target-a"]),
                NodeAcceleratorObservation::unavailable("node-a")
            ),
            Err(TargetCapabilitySelectionError::AcceleratorUnavailable {
                execution_target_id: "target-a".to_owned(),
            })
        );
    }

    #[test]
    fn simulated_targets_select_the_hardware_class_of_their_declared_machine() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let supported = [
            memory_class("nvidia-unified", AcceleratorMemoryArchitecture::Unified),
            memory_class("nvidia-dedicated", AcceleratorMemoryArchitecture::Dedicated),
            supported_class("apple", None, &["apple"]),
        ];
        let declared = |machine: &str| {
            let description = MachineDescription::simulated(&[machine.to_owned()], "amd64")
                .expect("simulated machine");
            let platform = description.platform().clone();
            let snapshot = description
                .render(&[], SystemTime::now())
                .expect("rendered snapshot");
            let target = simulated_target(&platform);
            let inventory = TargetCapabilityReconciler::reconcile(
                [&target],
                [RuntimePlatformObservation::available("target-a", platform)],
                [NodeAcceleratorObservation::from_hardware_snapshot(
                    "node-a", &snapshot, false,
                )],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            );
            let capabilities =
                select_target_capabilities(&target, &inventory, std::time::SystemTime::now())
                    .expect("target capabilities");
            let class = selected_class_id(&supported, &capabilities).expect("hardware class");
            (class, capabilities.memory_pool_total_bytes())
        };

        assert_eq!(
            declared("gb10"),
            ("nvidia-unified".to_owned(), Some(130_596_048_896))
        );
        assert_eq!(
            declared("rtx-5090"),
            ("nvidia-dedicated".to_owned(), Some(32_607 * 1024 * 1024))
        );
        assert_eq!(declared("mac-64g"), ("apple".to_owned(), Some(64 * GIB)));
    }

    #[test]
    fn device_model_classes_match_only_targets_whose_every_device_reports_a_listed_model() {
        let rtx_5090 = || {
            let mut support = memory_class("gpu-a", AcceleratorMemoryArchitecture::Dedicated);
            support
                .hardware_class
                .selector
                .as_mut()
                .expect("selector")
                .device_models = vec!["NVIDIA GeForce RTX 5090".to_owned()];
            support
        };
        let nested = [
            memory_class("nvidia-dedicated", AcceleratorMemoryArchitecture::Dedicated),
            rtx_5090(),
        ];
        let only_device_model = [rtx_5090()];
        let class_for = |supported: &[SupportedHardwareClass], gpus: &[&str], devices: &[&str]| {
            let description = MachineDescription::simulated(
                &gpus.iter().map(|&gpu| gpu.to_owned()).collect::<Vec<_>>(),
                "amd64",
            )
            .expect("simulated machine");
            let platform = description.platform().clone();
            let snapshot = description
                .render(&[], SystemTime::now())
                .expect("rendered snapshot");
            let target = ExecutionTarget {
                accelerator_device_ids: devices.iter().map(|&device| device.to_owned()).collect(),
                ..simulated_target(&platform)
            };
            let inventory = TargetCapabilityReconciler::reconcile(
                [&target],
                [RuntimePlatformObservation::available("target-a", platform)],
                [NodeAcceleratorObservation::from_hardware_snapshot(
                    "node-a", &snapshot, false,
                )],
                TARGET_CAPABILITY_MAX_AGE,
            );
            let capabilities =
                select_target_capabilities(&target, &inventory, std::time::SystemTime::now())
                    .expect("target capabilities");
            selected_class_id(supported, &capabilities)
        };
        let no_match = Err(HardwareClassSelectionError::NoMatch {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            accelerator: "nvidia",
            memory_architecture: MemoryArchitecture::Dedicated,
        });

        // The device model class is the most specific match on its GPU only.
        assert_eq!(
            class_for(&nested, &["rtx-5090"], &["0"]),
            Ok("gpu-a".to_owned())
        );
        assert_eq!(
            class_for(&nested, &["rtx-4080"], &["0"]),
            Ok("nvidia-dedicated".to_owned())
        );
        // On a machine with both GPUs, each single-GPU target gets its own GPU's class, and a
        // target holding both falls back to the vendor class.
        let mixed = ["rtx-5090", "rtx-4080"];
        assert_eq!(class_for(&nested, &mixed, &["0"]), Ok("gpu-a".to_owned()));
        assert_eq!(
            class_for(&nested, &mixed, &["1"]),
            Ok("nvidia-dedicated".to_owned())
        );
        assert_eq!(
            class_for(&nested, &mixed, &["0", "1"]),
            Ok("nvidia-dedicated".to_owned())
        );

        // Support on the device model class alone leaves other GPUs without a class.
        assert_eq!(
            class_for(&only_device_model, &["rtx-5090"], &["0"]),
            Ok("gpu-a".to_owned())
        );
        assert_eq!(
            class_for(&only_device_model, &["rtx-4080"], &["0"]),
            no_match
        );
        assert_eq!(
            class_for(&only_device_model, &["rtx-pro-6000"], &["0"]),
            no_match
        );

        // A target whose devices are unknown never matches a device model condition.
        let unreported = SelectedTargetCapabilities {
            device_models: vec![String::new()],
            ..nvidia_target(&ExecutionPlatform::linux_amd64(), "580.95.05")
        };
        assert_eq!(
            selected_class_id(&nested, &unreported),
            Ok("nvidia-dedicated".to_owned())
        );
        assert_eq!(
            selected_class_id(
                &nested,
                &nvidia_target(&ExecutionPlatform::linux_amd64(), "580.95.05")
            ),
            Ok("nvidia-dedicated".to_owned())
        );
    }

    #[test]
    fn hardware_class_selection_matches_unified_dedicated_and_unset_memory_architecture() {
        let amd64 = ExecutionPlatform::linux_amd64();
        let unified_target = SelectedTargetCapabilities {
            memory_architecture: MemoryArchitecture::Unified,
            ..nvidia_target(&ExecutionPlatform::linux_arm64(), "580.95.05")
        };
        let dedicated_target = nvidia_target(&amd64, "580.95.05");

        // Both memory classes nested in the broad class: each target picks its own narrower class.
        let nested = [
            supported_class("nvidia", None, &["nvidia"]),
            memory_class("nvidia-unified", AcceleratorMemoryArchitecture::Unified),
            memory_class("nvidia-dedicated", AcceleratorMemoryArchitecture::Dedicated),
        ];
        assert_eq!(
            selected_class_id(&nested, &unified_target),
            Ok("nvidia-unified".to_owned())
        );
        assert_eq!(
            selected_class_id(&nested, &dedicated_target),
            Ok("nvidia-dedicated".to_owned())
        );

        // An unset memory architecture matches both.
        let broad = [supported_class("nvidia", None, &["nvidia"])];
        assert_eq!(
            selected_class_id(&broad, &unified_target),
            Ok("nvidia".to_owned())
        );
        assert_eq!(
            selected_class_id(&broad, &dedicated_target),
            Ok("nvidia".to_owned())
        );

        // A unified-only configuration cannot run on dedicated memory.
        let unified_only = [memory_class(
            "nvidia-unified",
            AcceleratorMemoryArchitecture::Unified,
        )];
        assert_eq!(
            selected_class_id(&unified_only, &dedicated_target),
            Err(HardwareClassSelectionError::NoMatch {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
                memory_architecture: MemoryArchitecture::Dedicated,
            })
        );
    }

    #[test]
    fn hardware_class_selection_prefers_the_narrower_nested_class() {
        let arm64 = ExecutionPlatform::linux_arm64();
        let supported = [
            supported_class("gpu", None, &["nvidia", "apple"]),
            supported_class("nvidia-arm64", Some(&arm64), &["nvidia"]),
            supported_class("nvidia", None, &["nvidia"]),
        ];

        assert_eq!(
            selected_class_id(&supported, &nvidia_target(&arm64, "580.95.05")),
            Ok("nvidia-arm64".to_owned())
        );
        assert_eq!(
            selected_class_id(
                &supported,
                &nvidia_target(&ExecutionPlatform::linux_amd64(), "580.95.05")
            ),
            Ok("nvidia".to_owned())
        );
        assert_eq!(
            selected_class_id(&supported, &apple_target()),
            Ok("gpu".to_owned())
        );
    }

    #[test]
    fn hardware_class_selection_reports_no_match_and_ambiguity() {
        let nvidia_only = [supported_class("nvidia", None, &["nvidia"])];
        let error = selected_class_id(&nvidia_only, &apple_target())
            .expect_err("an apple target matches no nvidia class");
        assert_eq!(
            error.to_string(),
            "no supported hardware class matches target platform darwin/arm64 with apple accelerators and unified memory"
        );

        let arm64 = ExecutionPlatform::linux_arm64();
        let overlapping = [
            supported_class("arm64-gpu", Some(&arm64), &["nvidia", "apple"]),
            supported_class("nvidia", None, &["nvidia"]),
        ];
        assert_eq!(
            selected_class_id(&overlapping, &nvidia_target(&arm64, "580.95.05")),
            Err(HardwareClassSelectionError::Ambiguous {
                hardware_class_ids: vec!["arm64-gpu".to_owned(), "nvidia".to_owned()],
            })
        );
    }

    #[test]
    fn target_capabilities_carry_the_kinds_their_runtime_binding_realizes() {
        let platform = ExecutionPlatform::linux_amd64();
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                platform.clone(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );
        let kinds = |target: &ExecutionTarget| {
            select_target_capabilities(target, &inventory, std::time::SystemTime::now())
                .expect("target capabilities")
                .artifact_kinds
        };

        assert_eq!(kinds(&execution_target(&platform, &["target-a"])), OCI);
        assert_eq!(kinds(&simulated_target(&platform)), OCI);
        assert_eq!(
            WorkloadArtifactKind::realizable_by(&simulated_target(
                &ExecutionPlatform::darwin_arm64()
            )),
            NIX
        );
        let mut unbound = execution_target(&platform, &["target-a"]);
        unbound.runtime_binding = Some(RuntimeBinding::ModelRuntime(Default::default()));
        assert_eq!(kinds(&unbound), &[] as &[WorkloadArtifactKind]);
    }

    #[test]
    fn target_capabilities_accept_fresh_amd64_and_arm64_observations() {
        for platform in [
            ExecutionPlatform::linux_amd64(),
            ExecutionPlatform::linux_arm64(),
        ] {
            let target = execution_target(&platform, &["target-a"]);
            let inventory = TargetCapabilityReconciler::reconcile(
                [("target-a", "node-a"), ("target-b", "node-b")],
                [
                    RuntimePlatformObservation::available("target-a", platform.clone()),
                    RuntimePlatformObservation::available("target-b", platform.clone()),
                ],
                [
                    NodeAcceleratorObservation::nvidia("node-a", "581.1"),
                    NodeAcceleratorObservation::nvidia("node-b", "580.173.02"),
                ],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            );

            let selected =
                select_target_capabilities(&target, &inventory, std::time::SystemTime::now())
                    .expect("fresh exact target capabilities");
            assert_eq!(selected.platform, platform);
            assert_eq!(
                selected.accelerator,
                TargetAccelerator::Nvidia {
                    driver: driver("581.1")
                }
            );
        }
    }

    #[test]
    fn target_capabilities_reject_missing_stale_and_mismatched_observations_distinctly() {
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        assert_eq!(
            select_target_capabilities(
                &target,
                &TargetCapabilityInventory::empty(
                    dotcore_domain::timing::Timing::production()
                        .node_control
                        .capability_max_age
                ),
                std::time::SystemTime::now()
            ),
            Err(TargetCapabilitySelectionError::MissingObservation {
                execution_target_id: "target-a".to_owned(),
            })
        );

        let missing = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            std::iter::empty(),
            std::iter::empty(),
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );
        assert_eq!(
            select_target_capabilities(&target, &missing, std::time::SystemTime::now()),
            Err(TargetCapabilitySelectionError::PlatformUnavailable {
                execution_target_id: "target-a".to_owned(),
            })
        );

        let stale = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::stale(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::stale(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
            )],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );
        assert_eq!(
            select_target_capabilities(&target, &stale, std::time::SystemTime::now()),
            Err(TargetCapabilitySelectionError::StalePlatform {
                execution_target_id: "target-a".to_owned(),
            })
        );

        let mismatched = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_arm64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );
        assert_eq!(
            select_target_capabilities(&target, &mismatched, std::time::SystemTime::now()),
            Err(TargetCapabilitySelectionError::PlatformMismatch {
                execution_target_id: "target-a".to_owned(),
                observed_operating_system: "linux".to_owned(),
                observed_architecture: "arm64".to_owned(),
                required_operating_system: "linux".to_owned(),
                required_architecture: "amd64".to_owned(),
            })
        );
    }

    #[test]
    fn last_observed_capabilities_accept_stale_and_overdue_observations_but_nothing_else() {
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let nvidia = TargetAccelerator::Nvidia {
            driver: driver("580"),
        };

        let stale = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::stale(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::stale(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
            )],
            TARGET_CAPABILITY_MAX_AGE,
        );
        let selected =
            select_last_observed_target_capabilities(&target, &stale).expect("stale capabilities");
        assert_eq!(selected.platform, ExecutionPlatform::linux_amd64());
        assert_eq!(selected.accelerator, nvidia);

        let observed_at = SystemTime::UNIX_EPOCH;
        let overdue = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available_at(
                "target-a",
                ExecutionPlatform::linux_amd64(),
                observed_at,
            )],
            [NodeAcceleratorObservation::available_at(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
                observed_at,
            )],
            TARGET_CAPABILITY_MAX_AGE,
        );
        assert_eq!(
            select_last_observed_target_capabilities(&target, &overdue)
                .map(|selected| selected.accelerator),
            Ok(nvidia)
        );

        let missing = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            std::iter::empty(),
            std::iter::empty(),
            TARGET_CAPABILITY_MAX_AGE,
        );
        assert_eq!(
            select_last_observed_target_capabilities(&target, &missing),
            Err(TargetCapabilitySelectionError::PlatformUnavailable {
                execution_target_id: "target-a".to_owned(),
            })
        );

        let mismatched = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::stale(
                "target-a",
                ExecutionPlatform::linux_arm64(),
            )],
            [NodeAcceleratorObservation::stale(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
            )],
            TARGET_CAPABILITY_MAX_AGE,
        );
        assert_eq!(
            select_last_observed_target_capabilities(&target, &mismatched),
            Err(TargetCapabilitySelectionError::PlatformMismatch {
                execution_target_id: "target-a".to_owned(),
                observed_operating_system: "linux".to_owned(),
                observed_architecture: "arm64".to_owned(),
                required_operating_system: "linux".to_owned(),
                required_architecture: "amd64".to_owned(),
            })
        );
    }

    #[test]
    fn nvidia_targets_require_a_valid_driver_while_apple_targets_need_none() {
        let platform = ExecutionPlatform::linux_amd64();
        let target = execution_target(&platform, &["target-a"]);
        let inventory = |accelerator| {
            TargetCapabilityReconciler::reconcile(
                [("target-a", "node-a")],
                [RuntimePlatformObservation::available(
                    "target-a",
                    platform.clone(),
                )],
                [accelerator],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            )
        };

        assert_eq!(
            select_target_capabilities(
                &target,
                &inventory(NodeAcceleratorObservation::nvidia("node-a", "driver-580")),
                std::time::SystemTime::now()
            ),
            Err(TargetCapabilitySelectionError::NvidiaDriverUnavailable {
                execution_target_id: "target-a".to_owned(),
            })
        );
        assert_eq!(
            select_target_capabilities(
                &target,
                &inventory(NodeAcceleratorObservation::unavailable("node-a")),
                std::time::SystemTime::now()
            ),
            Err(TargetCapabilitySelectionError::AcceleratorUnavailable {
                execution_target_id: "target-a".to_owned(),
            })
        );
        assert_eq!(
            select_target_capabilities(
                &target,
                &inventory(NodeAcceleratorObservation::available(
                    "node-a",
                    NodeAcceleratorFacts::apple()
                )),
                std::time::SystemTime::now()
            )
            .map(|selected| selected.accelerator),
            Ok(TargetAccelerator::Apple)
        );
    }

    #[test]
    fn timestamped_capabilities_expire_for_admission() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let observed_at = now - TARGET_CAPABILITY_MAX_AGE - Duration::from_secs(1);
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available_at(
                "target-a",
                ExecutionPlatform::linux_amd64(),
                observed_at,
            )],
            [NodeAcceleratorObservation::available_at(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
                observed_at,
            )],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );

        assert_eq!(
            select_target_capabilities(&target, &inventory, now),
            Err(TargetCapabilitySelectionError::StalePlatform {
                execution_target_id: "target-a".to_owned(),
            })
        );

        let stale_accelerator = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available_at(
                "target-a",
                ExecutionPlatform::linux_amd64(),
                now,
            )],
            [NodeAcceleratorObservation::available_at(
                "node-a",
                NodeAcceleratorFacts::nvidia("580"),
                observed_at,
            )],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
        );
        assert_eq!(
            select_target_capabilities(&target, &stale_accelerator, now),
            Err(TargetCapabilitySelectionError::StaleAccelerator {
                execution_target_id: "target-a".to_owned(),
            })
        );
    }

    #[test]
    fn target_capabilities_carry_the_reported_memory_pool_total() {
        const TOTAL: u64 = 34_190_917_632;
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let total = |total: Option<u64>| {
            let inventory = TargetCapabilityReconciler::reconcile(
                [&target],
                [RuntimePlatformObservation::available(
                    "target-a",
                    ExecutionPlatform::linux_amd64(),
                )],
                [NodeAcceleratorObservation::nvidia("node-a", "580")
                    .with_device_memory("GPU-a", total, None)],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            );
            select_target_capabilities(&target, &inventory, std::time::SystemTime::now())
                .expect("selection never fails on the memory-pool total")
                .memory_pool_total_bytes()
        };

        assert_eq!(total(Some(TOTAL)), Some(TOTAL));
        assert_eq!(total(None), None);
    }

    pub(crate) fn execution_target(
        platform: &ExecutionPlatform,
        target_ids: &[&str],
    ) -> ExecutionTarget {
        let id = target_ids.first().expect("execution target ID");
        ExecutionTarget {
            execution_target_id: (*id).to_owned(),
            display_name: (*id).to_owned(),
            site_node_id: "node-a".to_owned(),
            accelerator_device_ids: vec!["0".to_owned()],
            platform: Some(platform.to_oci_platform()),
            runtime_binding: Some(crate::test_support::docker_model_runtime_binding()),
        }
    }

    pub(crate) fn simulated_target(platform: &ExecutionPlatform) -> ExecutionTarget {
        ExecutionTarget {
            runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                    Default::default(),
                )),
            })),
            ..execution_target(platform, &["target-a"])
        }
    }

    /// Supported hardware class with the given id, platform and accelerators, without minimums.
    fn supported_class(
        id: &str,
        platform: Option<&ExecutionPlatform>,
        accelerators: &[&str],
    ) -> SupportedHardwareClass {
        let mut hardware_class = vendor_hardware_class(id);
        let selector = hardware_class.selector.as_mut().expect("selector");
        selector.platform = platform.map(ExecutionPlatform::to_oci_platform);
        selector.accelerators = accelerators
            .iter()
            .map(|&vendor| vendor.to_owned())
            .collect();
        SupportedHardwareClass {
            hardware_class,
            hardware_requirements: None,
        }
    }

    /// Supported NVIDIA hardware class restricted to one memory architecture.
    fn memory_class(
        id: &str,
        architecture: AcceleratorMemoryArchitecture,
    ) -> SupportedHardwareClass {
        let mut support = supported_class(id, None, &["nvidia"]);
        support
            .hardware_class
            .selector
            .as_mut()
            .expect("selector")
            .set_memory_architecture(architecture);
        support
    }

    fn native_target(platform: &ExecutionPlatform) -> ExecutionTarget {
        ExecutionTarget {
            runtime_binding: Some(RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::NativeRuntime(
                    Default::default(),
                )),
            })),
            ..execution_target(platform, &["target-a"])
        }
    }

    fn selected_class_id(
        supported: &[SupportedHardwareClass],
        target: &SelectedTargetCapabilities,
    ) -> Result<String, HardwareClassSelectionError> {
        select_hardware_class(supported.to_vec(), target)
            .map(|support| support.hardware_class.hardware_class_id)
    }

    pub(crate) fn nvidia_target(
        platform: &ExecutionPlatform,
        version: &str,
    ) -> SelectedTargetCapabilities {
        SelectedTargetCapabilities {
            platform: platform.clone(),
            accelerator: TargetAccelerator::Nvidia {
                driver: driver(version),
            },
            artifact_kinds: OCI,
            memory_architecture: MemoryArchitecture::Dedicated,
            device_models: Vec::new(),
            capacity: TargetCapacity::default(),
        }
    }

    pub(crate) fn apple_target() -> SelectedTargetCapabilities {
        SelectedTargetCapabilities {
            platform: ExecutionPlatform::darwin_arm64(),
            accelerator: TargetAccelerator::Apple,
            artifact_kinds: ALL,
            memory_architecture: MemoryArchitecture::Unified,
            device_models: Vec::new(),
            capacity: TargetCapacity::default(),
        }
    }

    fn driver(version: &str) -> NvidiaDriverVersion {
        NvidiaDriverVersion::from_str(version).expect("valid test driver")
    }
}
