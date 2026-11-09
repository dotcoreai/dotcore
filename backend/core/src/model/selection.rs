//! Selects one approved engine artifact for a concrete execution target before a deployment
//! snapshot is accepted. Selection matches the artifact's selectors against the target's platform
//! and the vendor and memory architecture of its node's accelerators, in an artifact kind its
//! runtime can realize, for example the linux/arm64 NVIDIA selector of an OCI image on a Docker
//! target.
//!
//! Selection is deterministic, preserves the exact immutable artifact reference, and keeps the
//! concrete platform and vendor the target matched instead of the selector. The NVIDIA driver
//! minimum is a requirement of the matched selector, checked after the match. A retained
//! selection is revalidated against the target's current capabilities without choosing again.

use dotcore_domain::{
    Accelerator, ExecutionPlatform, MemoryArchitecture, NvidiaDriverMajorVersion,
    WorkloadArtifactKind,
};
use dotcore_proto::dotcore::{
    catalog::v1::{EngineArtifact, engine_artifact::Kind as ArtifactKind},
    site::v1::ExecutionTarget,
};
use dotcore_site::node::target_inventory::TargetCapabilityInventory;

use crate::hardware::selection::{
    SelectedTargetCapabilities, TargetAccelerator, TargetCapabilitySelectionError,
    memory_architecture_name, select_target_capabilities, selector_matches,
};

/// Revalidates a retained concrete selection against the current exact target observation.
pub(crate) fn validate_retained_target_capabilities(
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
    resolved: &crate::model::resolved::ResolvedModelLaunch,
) -> Result<(), RetainedTargetCapabilityError> {
    let current = select_target_capabilities(target, inventory)
        .map_err(RetainedTargetCapabilityError::Target)?;
    let retained = ExecutionPlatform::normalize(
        &resolved.engine.platform.operating_system,
        &resolved.engine.platform.architecture,
    )
    .map_err(RetainedTargetCapabilityError::InvalidRetainedPlatform)?;
    if resolved
        .engine
        .platform
        .variant
        .as_deref()
        .is_some_and(|variant| !variant.is_empty())
    {
        return Err(RetainedTargetCapabilityError::InvalidRetainedPlatform(
            dotcore_domain::ExecutionPlatformError::UnsupportedVariant {
                observed: resolved.engine.platform.variant.clone().unwrap_or_default(),
            },
        ));
    }
    if current.platform != retained {
        return Err(RetainedTargetCapabilityError::PlatformMismatch {
            target_operating_system: current.platform.operating_system().to_owned(),
            target_architecture: current.platform.architecture().to_owned(),
            retained_operating_system: retained.operating_system().to_owned(),
            retained_architecture: retained.architecture().to_owned(),
        });
    }
    let retained_accelerator = &resolved.engine.platform.accelerator;
    if Accelerator::from_catalog_name(retained_accelerator) != Some(current.accelerator.vendor()) {
        return Err(RetainedTargetCapabilityError::AcceleratorMismatch {
            target: current.accelerator.vendor().catalog_name(),
            retained: retained_accelerator.clone(),
        });
    }
    if let TargetAccelerator::Nvidia { driver } = &current.accelerator {
        let minimum =
            NvidiaDriverMajorVersion::new(resolved.engine.minimum_nvidia_driver_major_version)
                .map_err(RetainedTargetCapabilityError::InvalidDriverMinimum)?;
        if !driver.satisfies(minimum) {
            return Err(RetainedTargetCapabilityError::DriverBelowMinimum {
                observed_major: driver.major(),
                required_major: minimum.get(),
            });
        }
    }
    let retained_kind = resolved.engine.source.kind();
    if !current.artifact_kinds.contains(&retained_kind) {
        return Err(RetainedTargetCapabilityError::ArtifactKindUnrealizable {
            retained: retained_kind,
        });
    }
    // Sizing and launch requirements come from the retained hardware class, which must still
    // describe the target.
    let hardware_support = resolved.snapshot.hardware_support.as_ref();
    let hardware_class_id = hardware_support
        .map(|support| support.hardware_class_id.clone())
        .unwrap_or_default();
    if !hardware_support
        .and_then(|support| support.selector.as_ref())
        .is_some_and(|selector| selector_matches(selector, &current))
    {
        return Err(RetainedTargetCapabilityError::HardwareClassMismatch {
            hardware_class_id,
            operating_system: current.platform.operating_system().to_owned(),
            architecture: current.platform.architecture().to_owned(),
            accelerator: current.accelerator.vendor().catalog_name(),
            memory_architecture: current.memory_architecture,
        });
    }
    // The retained launch plan depends on the memory architecture, for example llama.cpp mmap.
    if current.memory_architecture != resolved.accelerator_memory {
        return Err(RetainedTargetCapabilityError::MemoryArchitectureMismatch {
            target: current.memory_architecture,
            retained: resolved.accelerator_memory,
        });
    }
    Ok(())
}

/// Failure while revalidating one retained engine choice without selecting a replacement.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum RetainedTargetCapabilityError {
    #[error("current target capability validation failed: {0}")]
    Target(#[source] TargetCapabilitySelectionError),

    #[error("retained engine platform is missing or unsupported: {0}")]
    InvalidRetainedPlatform(#[source] dotcore_domain::ExecutionPlatformError),

    #[error(
        "execution target is configured for {target_operating_system}/{target_architecture}, but the retained snapshot selects {retained_operating_system}/{retained_architecture}"
    )]
    PlatformMismatch {
        target_operating_system: String,
        target_architecture: String,
        retained_operating_system: String,
        retained_architecture: String,
    },

    #[error(
        "execution target reports {target} accelerators, but the retained snapshot matched {retained:?} accelerators"
    )]
    AcceleratorMismatch {
        target: &'static str,
        retained: String,
    },

    #[error("retained engine artifact has an invalid NVIDIA driver minimum: {0}")]
    InvalidDriverMinimum(#[source] dotcore_domain::NvidiaDriverVersionError),

    #[error(
        "target NVIDIA driver {observed_major} is below retained engine artifact minimum {required_major}"
    )]
    DriverBelowMinimum {
        observed_major: u32,
        required_major: u32,
    },

    #[error(
        "execution target reports {target:?} accelerator memory, but the retained snapshot was resolved for {retained:?}"
    )]
    MemoryArchitectureMismatch {
        target: MemoryArchitecture,
        retained: MemoryArchitecture,
    },

    #[error(
        "execution target's runtime cannot realize the retained snapshot's {retained:?} engine artifact"
    )]
    ArtifactKindUnrealizable { retained: WorkloadArtifactKind },

    #[error(
        "retained hardware class {hardware_class_id:?} no longer matches target platform {operating_system}/{architecture} with {accelerator} accelerators and {} memory",
        memory_architecture_name(*memory_architecture)
    )]
    HardwareClassMismatch {
        hardware_class_id: String,
        operating_system: String,
        architecture: String,
        accelerator: &'static str,
        memory_architecture: MemoryArchitecture,
    },
}

impl RetainedTargetCapabilityError {
    /// Whether the target's capabilities were missing, stale or unavailable rather than compared:
    /// a later observation may clear it. Every other failure is a mismatch between the target
    /// and the retained launch that only a new lifecycle generation re-evaluates.
    pub(crate) fn is_observation_gap(&self) -> bool {
        match self {
            Self::Target(error) => match error {
                TargetCapabilitySelectionError::MissingObservation { .. }
                | TargetCapabilitySelectionError::StalePlatform { .. }
                | TargetCapabilitySelectionError::PlatformUnavailable { .. }
                | TargetCapabilitySelectionError::StaleAccelerator { .. }
                | TargetCapabilitySelectionError::AcceleratorUnavailable { .. }
                | TargetCapabilitySelectionError::NvidiaDriverUnavailable { .. }
                | TargetCapabilitySelectionError::AcceleratorMemoryUnavailable { .. } => true,
                TargetCapabilitySelectionError::MissingTargetPlatform
                | TargetCapabilitySelectionError::PlatformMismatch { .. }
                | TargetCapabilitySelectionError::InvalidTargetPlatform(_) => false,
            },
            Self::InvalidRetainedPlatform(_)
            | Self::PlatformMismatch { .. }
            | Self::AcceleratorMismatch { .. }
            | Self::InvalidDriverMinimum(_)
            | Self::DriverBelowMinimum { .. }
            | Self::MemoryArchitectureMismatch { .. }
            | Self::ArtifactKindUnrealizable { .. }
            | Self::HardwareClassMismatch { .. } => false,
        }
    }
}

/// Selects exactly one approved artifact with a selector matching the target platform and
/// accelerator vendor, in an artifact kind both the target's runtime and the engine support. The
/// selected artifact drops its selectors and keeps the concrete facts the target matched and its
/// position among the approved artifacts.
pub(crate) fn select_approved_engine_artifact(
    approved: impl IntoIterator<Item = EngineArtifact>,
    target: &SelectedTargetCapabilities,
    engine_artifact_kinds: &[WorkloadArtifactKind],
) -> Result<SelectedEngineArtifact, EngineArtifactSelectionError> {
    let platform = &target.platform;
    let vendor = target.accelerator.vendor();
    let mut matches = approved
        .into_iter()
        .enumerate()
        .filter_map(|(position, artifact)| {
            let kind = engine_artifact_kind(&artifact)?;
            if !target.artifact_kinds.contains(&kind) || !engine_artifact_kinds.contains(&kind) {
                return None;
            }
            let minimum_nvidia_driver_major_version = artifact
                .selectors
                .iter()
                .find(|selector| selector_matches(selector, target))?
                .minimum_nvidia_driver_major_version;
            Some((position, artifact, minimum_nvidia_driver_major_version))
        });
    let no_match = || EngineArtifactSelectionError::NoMatch {
        operating_system: platform.operating_system().to_owned(),
        architecture: platform.architecture().to_owned(),
        accelerator: vendor.catalog_name(),
    };
    let Some((position, mut artifact, minimum_nvidia_driver_major_version)) = matches.next() else {
        return Err(no_match());
    };
    if matches.next().is_some() {
        return Err(EngineArtifactSelectionError::MultipleMatches {
            operating_system: platform.operating_system().to_owned(),
            architecture: platform.architecture().to_owned(),
            accelerator: vendor.catalog_name(),
        });
    }
    if let TargetAccelerator::Nvidia { driver } = &target.accelerator {
        let required_major = minimum_nvidia_driver_major_version;
        if !driver.satisfies(
            NvidiaDriverMajorVersion::new(required_major)
                .expect("validated NVIDIA engine artifact selector minimum driver"),
        ) {
            return Err(EngineArtifactSelectionError::DriverBelowMinimum {
                observed_major: driver.major(),
                required_major,
            });
        }
    }
    artifact.selectors.clear();
    Ok(SelectedEngineArtifact {
        position,
        artifact,
        target: MatchedEngineTarget {
            platform: platform.clone(),
            accelerator: vendor,
            minimum_nvidia_driver_major_version,
        },
    })
}

/// One approved engine artifact chosen for a target, without its selectors.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectedEngineArtifact {
    /// Position of the artifact among the approved artifacts given to the selection.
    pub(crate) position: usize,

    pub(crate) artifact: EngineArtifact,
    pub(crate) target: MatchedEngineTarget,
}

/// Concrete target facts an engine artifact's selector matched, retained instead of the selector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MatchedEngineTarget {
    pub(crate) platform: ExecutionPlatform,
    pub(crate) accelerator: Accelerator,
    /// Driver minimum of the matched selector; zero for vendors other than NVIDIA.
    pub(crate) minimum_nvidia_driver_major_version: u32,
}

/// Failures while selecting an approved artifact for one concrete target capability.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum EngineArtifactSelectionError {
    #[error(
        "no approved engine artifact supports target platform {operating_system}/{architecture} with {accelerator} accelerators in an artifact kind the target's runtime can realize"
    )]
    NoMatch {
        operating_system: String,
        architecture: String,
        accelerator: &'static str,
    },

    #[error(
        "multiple approved engine artifacts support target platform {operating_system}/{architecture} with {accelerator} accelerators"
    )]
    MultipleMatches {
        operating_system: String,
        architecture: String,
        accelerator: &'static str,
    },

    #[error(
        "target NVIDIA driver {observed_major} is below approved engine artifact minimum {required_major}"
    )]
    DriverBelowMinimum {
        observed_major: u32,
        required_major: u32,
    },
}

/// Returns the executable artifact kind an engine artifact declares. A Docker-delivered Nix
/// artifact yields a container image, so only container runtimes fit it.
pub(crate) fn engine_artifact_kind(artifact: &EngineArtifact) -> Option<WorkloadArtifactKind> {
    match artifact.kind.as_ref()? {
        ArtifactKind::Oci(_) => Some(WorkloadArtifactKind::Oci),
        ArtifactKind::Nix(nix) if dotcore_domain::nix::provides_docker_image(nix) => {
            Some(WorkloadArtifactKind::Oci)
        }
        ArtifactKind::Nix(_) => Some(WorkloadArtifactKind::Nix),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_domain::test_support::oci;
    use dotcore_domain::{
        Accelerator, ExecutionPlatform, MemoryArchitecture, WorkloadArtifactKind, test_support::nix,
    };
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineArtifact, NixDockerImage, Selector, engine_artifact::Kind as ArtifactKind,
            nix_artifact::Provides,
        },
        model::v1::model_deployment_snapshot::Source,
        observation::v1::AcceleratorMemoryArchitecture,
    };
    use dotcore_site::node::target_inventory::{
        NodeAcceleratorFacts, NodeAcceleratorObservation, RuntimePlatformObservation,
        TargetCapabilityReconciler,
    };

    use super::{
        EngineArtifactSelectionError, MatchedEngineTarget, RetainedTargetCapabilityError,
        SelectedTargetCapabilities, TargetAccelerator, TargetCapabilitySelectionError,
        select_approved_engine_artifact, select_target_capabilities,
        validate_retained_target_capabilities,
    };
    use crate::{
        hardware::selection::tests::{
            apple_target, execution_target, nvidia_target, simulated_target,
        },
        test_support::local_model_snapshot,
    };

    const OCI: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Oci];
    const NIX: &[WorkloadArtifactKind] = &[WorkloadArtifactKind::Nix];
    const ALL: &[WorkloadArtifactKind] = WorkloadArtifactKind::ALL;

    #[test]
    fn multi_platform_artifact_is_selected_with_the_matched_facts_for_each_supported_platform() {
        let artifact = multi_platform_artifact("cuda13-index");
        for platform in [
            ExecutionPlatform::linux_amd64(),
            ExecutionPlatform::linux_arm64(),
        ] {
            let selected = select_approved_engine_artifact(
                [artifact.clone()],
                &nvidia_target(&platform, "580.173.02"),
                OCI,
            )
            .expect("one compatible approved artifact");

            assert_eq!(selected.artifact.artifact_id, "cuda13-index");
            assert_eq!(selected.artifact.selectors, []);
            assert_eq!(selected.target, nvidia_match(&platform));
            assert_eq!(
                dotcore_domain::oci::engine_artifact_reference(&selected.artifact)
                    .and_then(|reference| reference.digest.as_ref())
                    .map(|digest| digest.encoded.as_str()),
                Some("1111111111111111111111111111111111111111111111111111111111111111")
            );
        }
    }

    #[test]
    fn separate_single_platform_artifacts_select_only_the_matching_approved_artifact() {
        let amd64 = single_platform_artifact("cuda13-amd64", "amd64", 'a');
        let arm64 = single_platform_artifact("cuda13-arm64", "arm64", 'b');

        for (platform, expected_id, expected_digest) in [
            (
                ExecutionPlatform::linux_amd64(),
                "cuda13-amd64",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
            (
                ExecutionPlatform::linux_arm64(),
                "cuda13-arm64",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        ] {
            let selected = select_approved_engine_artifact(
                [amd64.clone(), arm64.clone()],
                &nvidia_target(&platform, "580"),
                OCI,
            )
            .expect("matching single-platform artifact");

            assert_eq!(selected.artifact.artifact_id, expected_id);
            assert_eq!(
                dotcore_domain::oci::engine_artifact_reference(&selected.artifact)
                    .and_then(|reference| reference.digest.as_ref())
                    .map(|digest| digest.encoded.as_str()),
                Some(expected_digest)
            );
            assert_eq!(selected.target, nvidia_match(&platform));
        }
    }

    #[test]
    fn no_match_overlapping_match_and_driver_minimum_are_distinct_typed_failures() {
        let amd64 = single_platform_artifact("cuda13-amd64", "amd64", 'a');
        assert_eq!(
            select_approved_engine_artifact(
                [amd64.clone()],
                &nvidia_target(&ExecutionPlatform::linux_arm64(), "580"),
                OCI,
            ),
            Err(EngineArtifactSelectionError::NoMatch {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                accelerator: "nvidia",
            })
        );
        assert_eq!(
            select_approved_engine_artifact(
                [amd64.clone(), amd64.clone()],
                &nvidia_target(&ExecutionPlatform::linux_amd64(), "580"),
                OCI,
            ),
            Err(EngineArtifactSelectionError::MultipleMatches {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
            })
        );
        assert_eq!(
            select_approved_engine_artifact(
                [amd64],
                &nvidia_target(&ExecutionPlatform::linux_amd64(), "579.99"),
                OCI,
            ),
            Err(EngineArtifactSelectionError::DriverBelowMinimum {
                observed_major: 579,
                required_major: 580,
            })
        );
    }

    #[test]
    fn darwin_simulated_target_on_a_declared_mac_selects_the_apple_entry() {
        let darwin = ExecutionPlatform::darwin_arm64();
        let target = simulated_target(&darwin);
        let mac = MachineDescription::named("mac-64g")
            .expect("named machine")
            .render(&[], std::time::SystemTime::now())
            .expect("rendered snapshot");
        let inventory = TargetCapabilityReconciler::reconcile(
            [&target],
            [RuntimePlatformObservation::available(
                "target-a",
                darwin.clone(),
            )],
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "node-a", &mac, false,
            )],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        let capabilities =
            select_target_capabilities(&target, &inventory).expect("Apple target capabilities");
        assert_eq!(capabilities.accelerator, TargetAccelerator::Apple);
        assert_eq!(
            capabilities.memory_architecture,
            MemoryArchitecture::Unified
        );

        assert_eq!(capabilities.artifact_kinds, NIX);

        let metal = nix_artifact("metal", "darwin", "arm64", "apple");
        let selected = select_approved_engine_artifact(
            [
                single_platform_artifact("cuda13-arm64", "arm64", 'b'),
                metal.clone(),
            ],
            &capabilities,
            ALL,
        )
        .expect("Apple selector without any NVIDIA driver");

        assert_eq!(
            selected.artifact,
            EngineArtifact {
                selectors: Vec::new(),
                ..metal
            }
        );
        assert_eq!(
            selected.target,
            MatchedEngineTarget {
                platform: darwin,
                accelerator: Accelerator::Apple,
                minimum_nvidia_driver_major_version: 0,
            }
        );
    }

    #[test]
    fn artifact_selectors_match_the_target_memory_architecture() {
        let platform = ExecutionPlatform::linux_amd64();
        let mut unified_only = single_platform_artifact("unified", "amd64", '1');
        unified_only.selectors[0].set_memory_architecture(AcceleratorMemoryArchitecture::Unified);

        assert!(matches!(
            select_approved_engine_artifact(
                [unified_only.clone()],
                &nvidia_target(&platform, "580.95.05"),
                OCI
            ),
            Err(EngineArtifactSelectionError::NoMatch { .. })
        ));
        let unified_target = SelectedTargetCapabilities {
            memory_architecture: MemoryArchitecture::Unified,
            ..nvidia_target(&platform, "580.95.05")
        };
        assert_eq!(
            select_approved_engine_artifact([unified_only], &unified_target, OCI)
                .expect("unified artifact")
                .artifact
                .artifact_id,
            "unified"
        );
    }

    #[test]
    fn selection_rejects_nix_on_docker_targets_and_oci_on_native_kind_sets() {
        let linux = ExecutionPlatform::linux_amd64();
        let docker = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                linux.clone(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        let docker = select_target_capabilities(&execution_target(&linux, &["target-a"]), &docker)
            .expect("Docker target capabilities");
        let linux_nix = nix_artifact("cuda-nix", "linux", "amd64", "nvidia");
        let linux_oci = single_platform_artifact("cuda13-amd64", "amd64", 'a');

        assert_eq!(docker.artifact_kinds, OCI);
        assert_eq!(
            select_approved_engine_artifact([linux_nix.clone()], &docker, ALL),
            Err(EngineArtifactSelectionError::NoMatch {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
            })
        );
        assert_eq!(
            select_approved_engine_artifact([linux_oci.clone(), linux_nix.clone()], &docker, ALL)
                .map(|selected| selected.artifact.artifact_id),
            Ok("cuda13-amd64".to_owned())
        );

        let native = SelectedTargetCapabilities {
            artifact_kinds: NIX,
            ..docker
        };
        assert_eq!(
            select_approved_engine_artifact([linux_oci.clone()], &native, ALL),
            Err(EngineArtifactSelectionError::NoMatch {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
            })
        );
        assert_eq!(
            select_approved_engine_artifact([linux_oci, linux_nix], &native, ALL)
                .map(|selected| selected.artifact.artifact_id),
            Ok("cuda-nix".to_owned())
        );
    }

    #[test]
    fn docker_delivered_nix_artifacts_fit_docker_targets_only() {
        let linux = ExecutionPlatform::linux_amd64();
        let docker = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                linux.clone(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        let docker = select_target_capabilities(&execution_target(&linux, &["target-a"]), &docker)
            .expect("Docker target capabilities");
        let mut image = nix_artifact("cuda-image", "linux", "amd64", "nvidia");
        let Some(ArtifactKind::Nix(nix)) = image.kind.as_mut() else {
            unreachable!("Nix fixture artifact");
        };
        nix.provides = Some(Provides::DockerImage(NixDockerImage {}));

        assert_eq!(
            select_approved_engine_artifact([image.clone()], &docker, OCI)
                .map(|selected| selected.artifact.artifact_id),
            Ok("cuda-image".to_owned())
        );
        let native = SelectedTargetCapabilities {
            artifact_kinds: NIX,
            ..docker
        };
        assert_eq!(
            select_approved_engine_artifact([image], &native, ALL),
            Err(EngineArtifactSelectionError::NoMatch {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
            })
        );
    }

    #[test]
    fn apple_targets_never_select_nvidia_entries() {
        let mut nvidia_on_darwin = single_platform_artifact("cuda", "arm64", 'a');
        nvidia_on_darwin.selectors[0].platform =
            Some(ExecutionPlatform::darwin_arm64().to_oci_platform());

        assert_eq!(
            select_approved_engine_artifact([nvidia_on_darwin], &apple_target(), OCI),
            Err(EngineArtifactSelectionError::NoMatch {
                operating_system: "darwin".to_owned(),
                architecture: "arm64".to_owned(),
                accelerator: "apple",
            })
        );
    }

    #[test]
    fn selection_rejects_artifact_kinds_the_target_runtime_or_engine_cannot_realize() {
        let artifact = single_platform_artifact("cuda13-amd64", "amd64", 'a');
        let no_match = Err(EngineArtifactSelectionError::NoMatch {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            accelerator: "nvidia",
        });
        let mut unbound = nvidia_target(&ExecutionPlatform::linux_amd64(), "580");
        unbound.artifact_kinds = &[];

        assert_eq!(
            select_approved_engine_artifact([artifact.clone()], &unbound, OCI),
            no_match
        );
        assert_eq!(
            select_approved_engine_artifact(
                [artifact],
                &nvidia_target(&ExecutionPlatform::linux_amd64(), "580"),
                &[],
            ),
            no_match
        );
    }

    #[test]
    fn retained_selection_rejects_changed_target_platform_and_downgraded_driver() {
        let snapshot = local_model_snapshot("fixture-model");
        let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("resolved retained snapshot");

        let arm64_target = execution_target(&ExecutionPlatform::linux_arm64(), &["target-a"]);
        let arm64_inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_arm64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        assert!(matches!(
            validate_retained_target_capabilities(&arm64_target, &arm64_inventory, &resolved),
            Err(RetainedTargetCapabilityError::PlatformMismatch { .. })
        ));

        let amd64_target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let downgraded_inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "579")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        assert_eq!(
            validate_retained_target_capabilities(&amd64_target, &downgraded_inventory, &resolved),
            Err(RetainedTargetCapabilityError::DriverBelowMinimum {
                observed_major: 579,
                required_major: 580,
            })
        );
    }

    #[test]
    fn retained_selection_rejects_a_changed_accelerator_vendor() {
        let snapshot = local_model_snapshot("fixture-model");
        let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("resolved retained snapshot");
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [
                NodeAcceleratorObservation::available("node-a", NodeAcceleratorFacts::apple())
                    .with_memory_architecture(Some(MemoryArchitecture::Dedicated)),
            ],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );

        assert_eq!(
            validate_retained_target_capabilities(&target, &inventory, &resolved),
            Err(RetainedTargetCapabilityError::AcceleratorMismatch {
                target: "apple",
                retained: "nvidia".to_owned(),
            })
        );
    }

    #[test]
    fn retained_selection_rejects_an_engine_kind_the_target_cannot_realize() {
        let snapshot = local_model_snapshot("fixture-model");
        let mut resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("resolved retained snapshot");
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        assert_eq!(
            validate_retained_target_capabilities(&target, &inventory, &resolved),
            Ok(())
        );

        resolved.engine.source = crate::model::resolved::ResolvedEngineSource::Nix {
            flake: "self".to_owned(),
            package: "engines.llama-cpp.native".to_owned(),
            executable: "llama-server".to_owned(),
        };

        assert_eq!(
            validate_retained_target_capabilities(&target, &inventory, &resolved),
            Err(RetainedTargetCapabilityError::ArtifactKindUnrealizable {
                retained: WorkloadArtifactKind::Nix,
            })
        );
    }

    #[test]
    fn only_missing_stale_or_unavailable_observations_are_observation_gaps() {
        let target_id = || "target-a".to_owned();
        let observation_gaps = [
            TargetCapabilitySelectionError::MissingObservation {
                execution_target_id: target_id(),
            },
            TargetCapabilitySelectionError::StalePlatform {
                execution_target_id: target_id(),
            },
            TargetCapabilitySelectionError::PlatformUnavailable {
                execution_target_id: target_id(),
            },
            TargetCapabilitySelectionError::StaleAccelerator {
                execution_target_id: target_id(),
            },
            TargetCapabilitySelectionError::AcceleratorUnavailable {
                execution_target_id: target_id(),
            },
        ]
        .map(RetainedTargetCapabilityError::Target);
        let mismatches = [
            RetainedTargetCapabilityError::Target(
                TargetCapabilitySelectionError::PlatformMismatch {
                    execution_target_id: target_id(),
                    observed_operating_system: "darwin".to_owned(),
                    observed_architecture: "arm64".to_owned(),
                    required_operating_system: "linux".to_owned(),
                    required_architecture: "amd64".to_owned(),
                },
            ),
            RetainedTargetCapabilityError::DriverBelowMinimum {
                observed_major: 580,
                required_major: 999,
            },
            RetainedTargetCapabilityError::AcceleratorMismatch {
                target: "apple",
                retained: "nvidia".to_owned(),
            },
        ];

        assert!(
            observation_gaps
                .iter()
                .all(RetainedTargetCapabilityError::is_observation_gap)
        );
        assert!(
            !mismatches
                .iter()
                .any(RetainedTargetCapabilityError::is_observation_gap)
        );
    }

    #[test]
    fn retained_selection_ignores_a_changed_memory_pool_total() {
        let snapshot = local_model_snapshot("fixture-model");
        let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("resolved retained snapshot");
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [
                NodeAcceleratorObservation::nvidia("node-a", "580").with_device_memory(
                    "GPU-0",
                    Some(25_769_803_776),
                    None,
                ),
            ],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );

        assert_eq!(
            validate_retained_target_capabilities(&target, &inventory, &resolved),
            Ok(())
        );
    }

    #[test]
    fn retained_selection_rejects_changed_memory_architecture() {
        let snapshot = local_model_snapshot("fixture-model");
        let resolved = crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
            .expect("resolved retained snapshot");
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = |memory_architecture| {
            TargetCapabilityReconciler::reconcile(
                [("target-a", "node-a")],
                [RuntimePlatformObservation::available(
                    "target-a",
                    ExecutionPlatform::linux_amd64(),
                )],
                [NodeAcceleratorObservation::nvidia("node-a", "580")
                    .with_memory_architecture(Some(memory_architecture))],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
                std::time::SystemTime::now(),
            )
        };

        assert_eq!(resolved.accelerator_memory, MemoryArchitecture::Dedicated);
        assert_eq!(
            validate_retained_target_capabilities(
                &target,
                &inventory(MemoryArchitecture::Dedicated),
                &resolved
            ),
            Ok(())
        );
        assert_eq!(
            validate_retained_target_capabilities(
                &target,
                &inventory(MemoryArchitecture::Unified),
                &resolved
            ),
            Err(RetainedTargetCapabilityError::MemoryArchitectureMismatch {
                target: MemoryArchitecture::Unified,
                retained: MemoryArchitecture::Dedicated,
            })
        );
    }

    #[test]
    fn retained_selection_rejects_a_hardware_class_that_no_longer_matches_the_target() {
        let target = execution_target(&ExecutionPlatform::linux_amd64(), &["target-a"]);
        let inventory = TargetCapabilityReconciler::reconcile(
            [("target-a", "node-a")],
            [RuntimePlatformObservation::available(
                "target-a",
                ExecutionPlatform::linux_amd64(),
            )],
            [NodeAcceleratorObservation::nvidia("node-a", "580")
                .with_memory_architecture(Some(MemoryArchitecture::Dedicated))],
            dotcore_domain::timing::Timing::production()
                .node_control
                .capability_max_age,
            std::time::SystemTime::now(),
        );
        let resolved_with_class_memory = |architecture| {
            let mut snapshot = local_model_snapshot("fixture-model");
            let Some(Source::Local(local)) = snapshot.source.as_mut() else {
                panic!("local fixture snapshot");
            };
            let support = local.hardware_support.as_mut().expect("hardware support");
            support
                .selector
                .as_mut()
                .expect("hardware class selector")
                .set_memory_architecture(architecture);
            crate::model::resolved::ResolvedModelLaunch::from_snapshot(&snapshot)
                .expect("resolved retained snapshot")
        };

        let matching = resolved_with_class_memory(AcceleratorMemoryArchitecture::Dedicated);
        assert_eq!(
            validate_retained_target_capabilities(&target, &inventory, &matching),
            Ok(())
        );

        let stale = resolved_with_class_memory(AcceleratorMemoryArchitecture::Unified);
        let error = validate_retained_target_capabilities(&target, &inventory, &stale)
            .expect_err("stale hardware class");
        let hardware_class_id = stale
            .snapshot
            .hardware_support
            .as_ref()
            .expect("hardware support")
            .hardware_class_id
            .clone();
        assert_eq!(
            error.to_string(),
            format!(
                "retained hardware class {hardware_class_id:?} no longer matches target platform linux/amd64 with nvidia accelerators and dedicated memory"
            )
        );
        assert_eq!(
            error,
            RetainedTargetCapabilityError::HardwareClassMismatch {
                hardware_class_id,
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                accelerator: "nvidia",
                memory_architecture: MemoryArchitecture::Dedicated,
            }
        );
    }

    fn multi_platform_artifact(id: &str) -> EngineArtifact {
        let mut artifact = single_platform_artifact(id, "amd64", '1');
        artifact.selectors.push(Selector {
            platform: Some(ExecutionPlatform::linux_arm64().to_oci_platform()),
            accelerators: vec!["nvidia".to_owned()],
            minimum_nvidia_driver_major_version: 580,
            ..Default::default()
        });
        artifact
    }

    fn single_platform_artifact(
        id: &str,
        architecture: &str,
        digest_character: char,
    ) -> EngineArtifact {
        oci::engine_artifact(
            id,
            Some("registry.dotcore.test"),
            "llama-cpp/server-cuda13",
            &digest_character.to_string().repeat(64),
            "linux",
            architecture,
            "nvidia",
        )
    }

    fn nix_artifact(
        id: &str,
        operating_system: &str,
        architecture: &str,
        accelerator: &str,
    ) -> EngineArtifact {
        nix::engine_artifact(
            id,
            &nix::locked_flake(),
            operating_system,
            architecture,
            accelerator,
        )
    }

    fn nvidia_match(platform: &ExecutionPlatform) -> MatchedEngineTarget {
        MatchedEngineTarget {
            platform: platform.clone(),
            accelerator: Accelerator::Nvidia,
            minimum_nvidia_driver_major_version: 580,
        }
    }
}
