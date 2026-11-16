//! Recommends where one catalog application can run now: the application targets that can take
//! it, best first.
//!
//! A target qualifies when its node reports it fresh, its runtime runs the platform the site
//! declares for it, and every service of the application approves that platform, as creating a
//! deployment there requires. Targets the application already runs or starts on come first, then
//! those whose accelerators the application prefers, then the site manager's. The answer is
//! advisory: it never admits, reserves, or places a deployment.

use std::time::SystemTime;

use dotcore_domain::ExecutionPlatform;
use dotcore_proto::dotcore::{
    application::v1::{ApplicationDeployment, ApplicationLifecycleState, ApplicationTargetOption},
    catalog::v1::Application,
    deployment::v1::ExactPlacement,
    site::v1::{ExecutionTarget, execution_target::RuntimeBinding},
};
use dotcore_site::node::target_inventory::{CapabilityObservation, TargetCapabilityInventory};

/// Returns the application targets among `targets` that can take `application` now, best first.
/// `site_manager_node_id` names the site manager's node, when it has one, and `deployments` are
/// the site's application deployments. A target matches the application's preference when it
/// holds accelerator devices of a vendor the application prefers; every target matches an
/// application with no preference.
pub(crate) fn recommend_application_targets(
    application: &Application,
    targets: &[ExecutionTarget],
    site_manager_node_id: Option<&str>,
    inventory: &TargetCapabilityInventory,
    deployments: &[ApplicationDeployment],
) -> Vec<ApplicationTargetOption> {
    let mut options = targets
        .iter()
        .filter(|target| can_take(application, target, inventory))
        .map(|target| {
            let placement = ExactPlacement {
                site_node_id: target.site_node_id.clone(),
                execution_target_id: target.execution_target_id.clone(),
            };
            ApplicationTargetOption {
                matches_preference: matches_preference(application, target, inventory),
                application_deployment_id: deployment_id(application, &placement, deployments),
                target: Some(placement),
            }
        })
        .collect::<Vec<_>>();
    options.sort_by_cached_key(|option| {
        let placement = option.target.clone().unwrap_or_default();
        (
            option.application_deployment_id.is_none(),
            !option.matches_preference,
            Some(placement.site_node_id.as_str()) != site_manager_node_id,
            placement.site_node_id,
            placement.execution_target_id,
        )
    });
    options
}

/// Whether `target` is an application target that can take the application now: freshly
/// observed, on its declared platform, which every service of the application approves.
fn can_take(
    application: &Application,
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
) -> bool {
    if !matches!(
        target.runtime_binding,
        Some(RuntimeBinding::ApplicationRuntime(_))
    ) {
        return false;
    }
    let Some(capabilities) = inventory.for_target(&target.execution_target_id) else {
        return false;
    };
    let Some(observed) = fresh(
        inventory,
        &capabilities.platform,
        capabilities.platform_observed_at,
    ) else {
        return false;
    };
    let declared = target
        .platform
        .as_ref()
        .and_then(|platform| ExecutionPlatform::try_from(platform).ok());
    if declared.as_ref() != Some(observed) {
        return false;
    }
    let platform = observed.to_oci_platform();
    application.graph.as_ref().is_some_and(|graph| {
        graph.services.iter().all(|service| {
            service
                .artifact
                .as_ref()
                .is_some_and(|artifact| artifact.supported_platforms.contains(&platform))
        })
    })
}

/// Whether `target` holds accelerator devices of a vendor the application prefers, or the
/// application prefers none.
fn matches_preference(
    application: &Application,
    target: &ExecutionTarget,
    inventory: &TargetCapabilityInventory,
) -> bool {
    if application.preferred_accelerators.is_empty() {
        return true;
    }
    if target.accelerator_device_ids.is_empty() {
        return false;
    }
    inventory
        .for_target(&target.execution_target_id)
        .and_then(|capabilities| {
            fresh(
                inventory,
                &capabilities.accelerator,
                capabilities.accelerator_observed_at,
            )
        })
        .is_some_and(|vendor| {
            application
                .preferred_accelerators
                .iter()
                .any(|preferred| preferred == vendor.catalog_name())
        })
}

/// The first running or starting deployment of the application placed at `placement`. Paused,
/// stopped, failed, unavailable and stopping deployments don't count.
fn deployment_id(
    application: &Application,
    placement: &ExactPlacement,
    deployments: &[ApplicationDeployment],
) -> Option<String> {
    deployments
        .iter()
        .find(|deployment| {
            let same_application = deployment
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.application_id == application.application_id);
            let active = matches!(
                deployment
                    .state
                    .as_ref()
                    .map(|state| state.lifecycle_state()),
                Some(
                    ApplicationLifecycleState::Queued
                        | ApplicationLifecycleState::Preparing
                        | ApplicationLifecycleState::Starting
                        | ApplicationLifecycleState::Running
                        | ApplicationLifecycleState::Ready
                )
            );
            deployment.placement.as_ref() == Some(placement) && same_application && active
        })
        .map(|deployment| deployment.application_deployment_id.clone())
}

/// The observed value when it is available and was fresh when the inventory was read.
fn fresh<'a, T>(
    inventory: &TargetCapabilityInventory,
    observation: &'a CapabilityObservation<T>,
    observed_at: Option<SystemTime>,
) -> Option<&'a T> {
    match observation {
        CapabilityObservation::Available(value) if inventory.is_fresh(observed_at) => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use dotcore_domain::timing::Timing;
    use dotcore_node_hardware::MachineDescription;
    use dotcore_proto::dotcore::{
        application::v1::{
            ApplicationDeployment, ApplicationDeploymentState, ApplicationLifecycleState,
            ResolvedApplicationDeploymentSnapshot,
        },
        catalog::v1::{
            Application, ApplicationGraph, ApplicationService, ApplicationServiceArtifact,
            OciPlatform,
        },
        deployment::v1::ExactPlacement,
        site::v1::{ExecutionTarget, execution_target::RuntimeBinding},
    };
    use dotcore_site::{
        initial::inputs::described_execution_targets,
        node::target_inventory::{
            NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityInventory,
            TargetCapabilityReconciler,
        },
    };

    use super::recommend_application_targets;

    use ApplicationLifecycleState::{Failed, Paused, Ready, Starting};

    /// Expected recommendations for one site of described machines.
    struct Case {
        name: &'static str,
        site: Site,
        application: Application,
        deployments: Vec<(&'static str, &'static str, ApplicationLifecycleState)>,
        /// Each expected target, best first: its node, whether it matches the application's
        /// preference, and the application's deployment there.
        expected: Vec<(&'static str, bool, Option<&'static str>)>,
    }

    #[test]
    fn application_targets_are_the_ones_that_can_take_the_application_best_first() {
        let cases = [
            Case {
                name: "the NVIDIA machine's Docker target holds its GPU, the Mac's holds none",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: vec![("z-gpu", true, None), ("a-mac", false, None)],
            },
            Case {
                name: "a Mac alone never matches",
                site: Site::of(&[("a-mac", MAC)]),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: vec![("a-mac", false, None)],
            },
            Case {
                name: "an NVIDIA machine's Docker target that holds no GPU doesn't match",
                site: Site::of(&[("z-gpu", NVIDIA)]).without_application_devices(),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: vec![("z-gpu", false, None)],
            },
            Case {
                name: "a site without application targets offers none",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]).without_application_targets(),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: Vec::new(),
            },
            Case {
                name: "a target running or starting the application comes first, with its deployment",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: vec![
                    ("a-mac", "app-on-mac", Starting),
                    ("a-mac", "app-on-mac-ready", Ready),
                ],
                expected: vec![("a-mac", false, Some("app-on-mac")), ("z-gpu", true, None)],
            },
            Case {
                name: "failed and paused deployments and other applications' don't count",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: vec![
                    ("a-mac", "app-failed", Failed),
                    ("a-mac", "app-paused", Paused),
                    ("a-mac", OTHER_APPLICATION_DEPLOYMENT, Ready),
                ],
                expected: vec![("z-gpu", true, None), ("a-mac", false, None)],
            },
            Case {
                name: "a machine that stopped reporting is dropped, even running the application",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]).stale("z-gpu"),
                application: application(&["nvidia"], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: vec![("z-gpu", "app-on-gpu", Ready)],
                expected: vec![("a-mac", false, None)],
            },
            Case {
                name: "without a preference every target matches, the site manager's first",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]).managed_by("z-gpu"),
                application: application(&[], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: vec![("z-gpu", true, None), ("a-mac", true, None)],
            },
            Case {
                name: "without a preference or a manager, by node",
                site: Site::of(&[("z-gpu", NVIDIA), ("a-mac", MAC)]),
                application: application(&[], &[LINUX_AMD64, LINUX_ARM64]),
                deployments: Vec::new(),
                expected: vec![("a-mac", true, None), ("z-gpu", true, None)],
            },
            Case {
                name: "a platform one service doesn't approve is left out",
                site: Site::of(&[("a-mac", MAC), ("z-gpu", NVIDIA)]),
                application: application(&["nvidia"], &[LINUX_AMD64]),
                deployments: Vec::new(),
                expected: vec![("z-gpu", true, None)],
            },
        ];

        for case in cases {
            let deployments = case
                .deployments
                .iter()
                .map(|(node, id, state)| case.site.deployment(node, id, *state))
                .collect::<Vec<_>>();
            let options = recommend_application_targets(
                &case.application,
                &case.site.targets,
                case.site.manager.as_deref(),
                &case.site.inventory,
                &deployments,
            );
            let actual = options
                .into_iter()
                .map(|option| {
                    let target = option.target.expect("a target");
                    (
                        target,
                        option.matches_preference,
                        option.application_deployment_id,
                    )
                })
                .collect::<Vec<_>>();
            let expected = case
                .expected
                .iter()
                .map(|(node, matches, deployment)| {
                    (
                        case.site.application_placement(node),
                        *matches,
                        deployment.map(str::to_owned),
                    )
                })
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{}", case.name);
        }
    }

    /// A site of described machines, each its own node, observed now unless made stale.
    struct Site {
        machines: Vec<(&'static str, MachineDescription)>,
        targets: Vec<ExecutionTarget>,
        manager: Option<String>,
        stale: Vec<&'static str>,
        inventory: TargetCapabilityInventory,
    }

    impl Site {
        /// Declares the targets site setup gives each named machine.
        fn of(machines: &[(&'static str, &str)]) -> Self {
            let machines = machines
                .iter()
                .map(|(node, yaml)| {
                    let description =
                        MachineDescription::from_yaml(node, yaml).expect("described machine");
                    (*node, description)
                })
                .collect::<Vec<_>>();
            let targets = machines
                .iter()
                .flat_map(|(node, description)| {
                    described_execution_targets(node, description).expect("described targets")
                })
                .collect();
            Self {
                machines,
                targets,
                manager: None,
                stale: Vec::new(),
                inventory: TargetCapabilityInventory::empty_now(),
            }
            .observed()
        }

        fn without_application_devices(mut self) -> Self {
            for target in self
                .targets
                .iter_mut()
                .filter(|target| is_application(target))
            {
                target.accelerator_device_ids.clear();
            }
            self.observed()
        }

        fn without_application_targets(mut self) -> Self {
            self.targets.retain(|target| !is_application(target));
            self.observed()
        }

        fn stale(mut self, node: &'static str) -> Self {
            self.stale.push(node);
            self.observed()
        }

        fn managed_by(mut self, node: &str) -> Self {
            self.manager = Some(node.to_owned());
            self
        }

        /// Observes every target's platform and every node's accelerators, long ago on stale
        /// nodes.
        fn observed(mut self) -> Self {
            let now = SystemTime::now();
            let max_age = Timing::test().node_control.capability_max_age;
            let reported_at = |node: &str| {
                if self.stale.contains(&node) {
                    now - 2 * max_age
                } else {
                    now
                }
            };
            let platforms = self
                .targets
                .iter()
                .map(|target| {
                    RuntimePlatformObservation::available_at(
                        target.execution_target_id.clone(),
                        target
                            .platform
                            .as_ref()
                            .expect("target platform")
                            .try_into()
                            .expect("supported platform"),
                        reported_at(&target.site_node_id),
                    )
                })
                .collect::<Vec<_>>();
            let accelerators = self
                .machines
                .iter()
                .map(|(node, description)| {
                    let snapshot = description
                        .render(&[], reported_at(node))
                        .expect("rendered snapshot");
                    NodeAcceleratorObservation::from_hardware_snapshot(
                        *node,
                        &snapshot,
                        self.stale.contains(node),
                    )
                })
                .collect::<Vec<_>>();
            self.inventory = TargetCapabilityReconciler::reconcile(
                &self.targets,
                platforms,
                accelerators,
                max_age,
                now,
            );
            self
        }

        /// The placement of the node's one application target.
        fn application_placement(&self, node: &str) -> ExactPlacement {
            let mut targets = self
                .targets
                .iter()
                .filter(|target| target.site_node_id == node && is_application(target));
            let target = targets.next().expect("an application target");
            assert!(targets.next().is_none(), "one application target on {node}");
            ExactPlacement {
                site_node_id: node.to_owned(),
                execution_target_id: target.execution_target_id.clone(),
            }
        }

        /// A deployment on the node's application target, of the fixture application unless
        /// its id says otherwise.
        fn deployment(
            &self,
            node: &str,
            id: &str,
            lifecycle_state: ApplicationLifecycleState,
        ) -> ApplicationDeployment {
            let application_id = if id == OTHER_APPLICATION_DEPLOYMENT {
                "other-application"
            } else {
                APPLICATION_ID
            };
            ApplicationDeployment {
                application_deployment_id: id.to_owned(),
                placement: Some(self.application_placement(node)),
                snapshot: Some(ResolvedApplicationDeploymentSnapshot {
                    application_id: application_id.to_owned(),
                    ..Default::default()
                }),
                state: Some(ApplicationDeploymentState {
                    lifecycle_state: lifecycle_state as i32,
                    ..Default::default()
                }),
                ..Default::default()
            }
        }
    }

    fn is_application(target: &ExecutionTarget) -> bool {
        matches!(
            target.runtime_binding,
            Some(RuntimeBinding::ApplicationRuntime(_))
        )
    }

    /// The fixture application with two services approving `platforms`, preferring
    /// `accelerators`.
    fn application(accelerators: &[&str], platforms: &[(&str, &str)]) -> Application {
        let platforms = platforms
            .iter()
            .map(|(operating_system, architecture)| OciPlatform {
                operating_system: (*operating_system).to_owned(),
                architecture: (*architecture).to_owned(),
                variant: None,
            })
            .collect::<Vec<_>>();
        let service = |name: &str| ApplicationService {
            service_name: name.to_owned(),
            artifact: Some(ApplicationServiceArtifact {
                kind: None,
                supported_platforms: platforms.clone(),
            }),
            ..Default::default()
        };
        Application {
            application_id: APPLICATION_ID.to_owned(),
            preferred_accelerators: accelerators
                .iter()
                .map(|accelerator| (*accelerator).to_owned())
                .collect(),
            graph: Some(ApplicationGraph {
                services: vec![service("web"), service("worker")],
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const APPLICATION_ID: &str = "fixture-application";

    /// The id of a deployment of another application.
    const OTHER_APPLICATION_DEPLOYMENT: &str = "other-application-deployment";

    const LINUX_AMD64: (&str, &str) = ("linux", "amd64");

    const LINUX_ARM64: (&str, &str) = ("linux", "arm64");

    /// A dedicated 32 GiB NVIDIA GPU beside 64 GiB of host memory.
    const NVIDIA: &str = r#"
platform: linux/amd64
cpuCount: 16
hostMemory: { totalBytes: 68719476736, idleUsedBytes: 4294967296 }
gpus:
  - id: GPU-fixture
    vendor: nvidia
    model: Fixture GPU
    driverVersion: "580.173.02"
    memory: { architecture: dedicated, totalBytes: 34359738368, reservedBytes: 536870912 }
"#;

    /// A 32 GiB Mac, 6 GiB in use at idle.
    const MAC: &str = r#"
platform: darwin/arm64
cpuCount: 10
hostMemory: { totalBytes: 34359738368, idleUsedBytes: 6442450944 }
gpus:
  - { id: apple-gpu-0, vendor: apple, model: Fixture Apple GPU, memory: { architecture: unified } }
"#;
}
