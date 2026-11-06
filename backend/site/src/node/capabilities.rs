//! Projects a node's infrastructure collection into execution-target capabilities: on the node
//! itself, so it can admit create and resume actions for its accepted model intent, and on the
//! manager, which admits creates from every node's latest report.
//!
//! Both sides use the same projection, and every target reads its node's reported hardware; a
//! simulated target's node reports its declared machine. It never re-stamps an older fact: a
//! failed probe reads as unavailable, and a collection older than the maximum age reads as stale.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use dotcore_domain::clock::Clock;

use dotcore_proto::dotcore::{
    observation::v1::runtime_observation,
    site::{
        private::v1::{NodeObservationSnapshot, node_hardware_observation},
        v1::{ExecutionTarget, execution_target},
    },
};

use crate::node::agent::NodeObservationFeed;
use crate::node::target_inventory::{
    NodeAcceleratorObservation, NodeMemoryObservation, RuntimePlatformObservation,
    TargetCapabilityInventory, TargetCapabilityReconciler, TargetCapabilitySource,
    TargetCapabilitySourceError,
};

/// Node-local capability source backed by the reporter's latest completed collection.
pub struct NodeTargetCapabilitySource {
    targets: Vec<ExecutionTarget>,
    observations: NodeObservationFeed,
    max_age: Duration,

    /// Backend clock whose wall time judges collections fresh, the clock that stamps them.
    clock: Arc<dyn Clock>,
}

impl NodeTargetCapabilitySource {
    /// Creates a source for the node's model execution targets. Collections older than
    /// `max_age` on `clock` are reported stale.
    pub fn new(
        targets: &[ExecutionTarget],
        observations: NodeObservationFeed,
        max_age: Duration,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            targets: targets
                .iter()
                .filter(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ModelRuntime(_))
                    )
                })
                .cloned()
                .collect(),
            observations,
            max_age,
            clock,
        }
    }

    fn inventory_at(
        &self,
        now: SystemTime,
    ) -> Result<TargetCapabilityInventory, TargetCapabilitySourceError> {
        let collected = self.observations.latest().ok_or_else(|| {
            TargetCapabilitySourceError::Observation(anyhow::anyhow!(
                "no node infrastructure observation has been collected yet"
            ))
        })?;
        // A collection time ahead of the local clock cannot prove freshness.
        let stale = now
            .duration_since(collected.collected_at)
            .map_or(true, |age| age > self.max_age);
        let mut inventory = TargetCapabilityInventory::default();
        for target in &self.targets {
            inventory.extend(node_target_capabilities(
                target,
                &collected.snapshot,
                stale,
                Some(collected.collected_at),
            )?);
        }
        Ok(inventory)
    }
}

impl TargetCapabilitySource for NodeTargetCapabilitySource {
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
        Box::pin(std::future::ready(self.inventory_at(self.clock.wall_now())))
    }
}

/// Projects one node collection into one target's capabilities: the target's reported runtime
/// outcome joined with its node's accelerator facts. The manager's admission and the node's own
/// admission share this projection; a missing `observed_at` reads as unavailable.
pub(crate) fn node_target_capabilities(
    target: &ExecutionTarget,
    snapshot: &NodeObservationSnapshot,
    stale: bool,
    observed_at: Option<SystemTime>,
) -> Result<TargetCapabilityInventory, TargetCapabilitySourceError> {
    let platform = node_runtime_platform(target, snapshot, stale, observed_at)?;
    let accelerator =
        node_accelerator_observation(&target.site_node_id, snapshot, stale, observed_at);
    Ok(TargetCapabilityReconciler::reconcile(
        [target],
        [platform],
        [accelerator],
    ))
}

/// Reads one node collection's memory, attributed to its node, at `observed_at`; every device and
/// the host count, whether or not a target of the node can use them.
pub(crate) fn node_memory_observation(
    node_id: &str,
    snapshot: &NodeObservationSnapshot,
    stale: bool,
    observed_at: Option<SystemTime>,
) -> NodeMemoryObservation {
    match snapshot
        .hardware
        .as_ref()
        .and_then(|hardware| hardware.result.as_ref())
    {
        Some(node_hardware_observation::Result::Snapshot(hardware)) => {
            NodeMemoryObservation::from_hardware_snapshot_at(node_id, hardware, stale, observed_at)
        }
        Some(node_hardware_observation::Result::Unavailable(_)) | None => {
            NodeMemoryObservation::unavailable(node_id)
        }
    }
}

/// Projects one node snapshot's accelerator facts, attributed to its node, at `observed_at`.
fn node_accelerator_observation(
    node_id: &str,
    snapshot: &NodeObservationSnapshot,
    stale: bool,
    observed_at: Option<SystemTime>,
) -> NodeAcceleratorObservation {
    match snapshot
        .hardware
        .as_ref()
        .and_then(|hardware| hardware.result.as_ref())
    {
        Some(node_hardware_observation::Result::Snapshot(hardware)) => {
            NodeAcceleratorObservation::from_hardware_snapshot_at(
                node_id,
                hardware,
                stale,
                observed_at,
            )
        }
        Some(node_hardware_observation::Result::Unavailable(_)) | None => {
            NodeAcceleratorObservation::unavailable(node_id)
        }
    }
}

/// Projects one node snapshot's runtime outcome for `target` at `observed_at`; only an
/// available outcome yields the target's configured platform.
fn node_runtime_platform(
    target: &dotcore_proto::dotcore::site::v1::ExecutionTarget,
    snapshot: &NodeObservationSnapshot,
    stale: bool,
    observed_at: Option<SystemTime>,
) -> Result<RuntimePlatformObservation, TargetCapabilitySourceError> {
    let platform = target
        .platform
        .as_ref()
        .ok_or_else(|| {
            TargetCapabilitySourceError::Observation(anyhow::anyhow!(
                "node execution target has no configured platform"
            ))
        })
        .and_then(|platform| {
            dotcore_domain::ExecutionPlatform::try_from(platform).map_err(|source| {
                TargetCapabilitySourceError::Observation(
                    anyhow::Error::new(source)
                        .context("read node execution-target platform for admission"),
                )
            })
        })?;
    let runtime = snapshot
        .runtimes
        .iter()
        .find(|runtime| runtime.execution_target_id == target.execution_target_id);
    Ok(match runtime.and_then(|runtime| runtime.result.as_ref()) {
        Some(runtime_observation::Result::Available(_)) if stale => observed_at
            .map(|observed_at| {
                RuntimePlatformObservation::stale_at(
                    target.execution_target_id.clone(),
                    platform,
                    observed_at,
                )
            })
            .unwrap_or_else(|| {
                RuntimePlatformObservation::unavailable(target.execution_target_id.clone())
            }),
        Some(runtime_observation::Result::Available(_)) => observed_at
            .map(|observed_at| {
                RuntimePlatformObservation::available_at(
                    target.execution_target_id.clone(),
                    platform,
                    observed_at,
                )
            })
            .unwrap_or_else(|| {
                RuntimePlatformObservation::unavailable(target.execution_target_id.clone())
            }),
        Some(runtime_observation::Result::Unsupported(_))
        | Some(runtime_observation::Result::Unavailable(_))
        | None => RuntimePlatformObservation::unavailable(target.execution_target_id.clone()),
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use dotcore_domain::{Accelerator, ExecutionPlatform, MemoryArchitecture};
    use dotcore_node_hardware::{MachineDescription, MachineWorkload};
    use dotcore_proto::dotcore::{
        catalog::v1::OciPlatform,
        observation::v1::{
            AcceleratorFacts, AcceleratorFactsSet, AcceleratorMemoryArchitecture,
            AcceleratorObservation, AcceleratorSignal, AcceleratorVendor, HardwareSnapshot,
            HostFacts, HostObservation, ObservationUnavailable, RuntimeObservation,
            accelerator_observation, host_observation, runtime_observation,
        },
        site::{
            private::v1::{
                NodeHardwareObservation, NodeObservationSnapshot, node_hardware_observation,
            },
            v1::{
                DockerRuntimeAdapter, ExecutionTarget, ModelRuntimeBinding, execution_target,
                model_runtime_binding,
            },
        },
    };

    use super::NodeTargetCapabilitySource;
    use crate::node::{
        agent::NodeObservationFeed,
        target_inventory::{
            CapabilityObservation, DeviceMemoryFacts, HostMemoryFacts, NodeMemoryFacts,
            TargetCapabilities,
        },
    };

    const TARGET_CAPABILITY_MAX_AGE: std::time::Duration =
        dotcore_domain::timing::Timing::production().target_capability_max_age;

    #[test]
    fn refreshed_collection_restores_capabilities_after_the_max_age() {
        let now = SystemTime::now();
        let feed = NodeObservationFeed::default();
        let source = NodeTargetCapabilitySource::new(
            &[target()],
            feed.clone(),
            TARGET_CAPABILITY_MAX_AGE,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        );
        feed.publish(
            crate::node::agent::NodeCollection::without_processes(available_snapshot()),
            1,
            now - TARGET_CAPABILITY_MAX_AGE - Duration::from_secs(1),
        );

        assert_eq!(
            capabilities_at(&source, now).platform,
            CapabilityObservation::Stale(ExecutionPlatform::linux_amd64())
        );

        feed.publish(
            crate::node::agent::NodeCollection::without_processes(available_snapshot()),
            2,
            now,
        );
        let refreshed = capabilities_at(&source, now);

        assert_eq!(
            refreshed.platform,
            CapabilityObservation::Available(ExecutionPlatform::linux_amd64())
        );
        assert_eq!(
            refreshed.accelerator,
            CapabilityObservation::Available(Accelerator::Nvidia)
        );
        assert!(matches!(
            &refreshed.nvidia_driver,
            CapabilityObservation::Available(driver) if driver.major() == 580
        ));
    }

    #[test]
    fn simulated_targets_read_their_nodes_declared_machine() {
        const GIB: u64 = 1024 * 1024 * 1024;
        const GB10_HOST_TOTAL: u64 = 130_596_048_896;
        const GB10_IDLE_USED: u64 = 7_938_146_304;
        const MAC_HOST_TOTAL: u64 = 64 * GIB;
        let now = SystemTime::now();
        let project = |machine: &str, platform: OciPlatform| {
            let rendered = MachineDescription::simulated(&[machine.to_owned()], "amd64")
                .expect("simulated machine")
                .render(&[], now)
                .expect("rendered snapshot");
            let mut simulated = target();
            simulated.platform = Some(platform);
            simulated.runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    )),
                },
            ));
            let snapshot = NodeObservationSnapshot {
                hardware: Some(NodeHardwareObservation {
                    result: Some(node_hardware_observation::Result::Snapshot(rendered)),
                }),
                ..available_snapshot()
            };
            super::node_target_capabilities(&simulated, &snapshot, false, Some(now))
                .expect("inventory")
                .for_target("target-one")
                .expect("target capabilities")
                .clone()
        };

        let gb10 = project(
            "gb10",
            OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "arm64".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(
            gb10.accelerator,
            CapabilityObservation::Available(Accelerator::Nvidia)
        );
        assert!(matches!(
            &gb10.nvidia_driver,
            CapabilityObservation::Available(driver) if driver.major() == 580
        ));
        assert_eq!(
            gb10.accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
        assert_eq!(
            host_memory(&gb10),
            HostMemoryFacts {
                total_bytes: GB10_HOST_TOTAL,
                available_bytes: GB10_HOST_TOTAL - GB10_IDLE_USED,
            }
        );

        let mac = project(
            "mac-64g",
            OciPlatform {
                operating_system: "darwin".to_owned(),
                architecture: "arm64".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(
            mac.platform,
            CapabilityObservation::Available(ExecutionPlatform::darwin_arm64())
        );
        assert_eq!(
            mac.accelerator,
            CapabilityObservation::Available(Accelerator::Apple)
        );
        assert_eq!(mac.nvidia_driver, CapabilityObservation::Unavailable);
        assert_eq!(
            mac.accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
        assert_eq!(
            host_memory(&mac),
            HostMemoryFacts {
                total_bytes: MAC_HOST_TOTAL,
                available_bytes: MAC_HOST_TOTAL - 8 * GIB,
            }
        );
    }

    #[test]
    fn declared_machines_project_like_a_real_reading_of_their_hardware() {
        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;
        const GB10_HOST_TOTAL: u64 = 130_596_048_896;
        const GB10_IDLE_USED: u64 = 7_938_146_304;
        // A real reading of the same hardware reports the same device IDs.
        const RTX_5090_ID: &str = "GPU-00000000-0000-0000-0000-000000005090";
        const GB10_ID: &str = "GPU-00000000-0000-0000-0000-00000000gb10";
        let now = SystemTime::now();
        let reading =
            |host: HostFacts, device_id: &str, facts: AcceleratorFacts| HardwareSnapshot {
                observed_at: Some(dotcore_node_hardware::hardware::timestamp_from(now)),
                host: Some(HostObservation {
                    result: Some(host_observation::Result::Facts(HostFacts {
                        host_name: "gpu1".to_owned(),
                        operating_system_or_kernel: "Linux 6.12".to_owned(),
                        cpu_count: 8,
                        cached_memory_bytes: Some(GIB),
                        ..host
                    })),
                }),
                accelerators: vec![AcceleratorObservation {
                    vendor: AcceleratorVendor::Nvidia as i32,
                    result: Some(accelerator_observation::Result::Facts(
                        AcceleratorFactsSet {
                            facts: vec![AcceleratorFacts {
                                vendor: AcceleratorVendor::Nvidia as i32,
                                device_id: device_id.to_owned(),
                                vendor_index: Some(0),
                                supported_signals: vec![AcceleratorSignal::Power as i32],
                                ..facts
                            }],
                        },
                    )),
                }],
            };
        let project = |hardware: HardwareSnapshot| {
            let snapshot = NodeObservationSnapshot {
                hardware: Some(NodeHardwareObservation {
                    result: Some(node_hardware_observation::Result::Snapshot(hardware)),
                }),
                ..available_snapshot()
            };
            super::node_target_capabilities(&target(), &snapshot, false, Some(now))
                .expect("inventory")
                .for_target("target-one")
                .expect("target capabilities")
                .clone()
        };
        let declared = |name: &str, workload: MachineWorkload| {
            MachineDescription::simulated(&[name.to_owned()], "amd64")
                .expect("simulated machine")
                .render(&[workload], now)
                .expect("rendered snapshot")
        };

        // A 10 GiB workload on the RTX 5090, read by NVML with the driver's 497 MiB reserved.
        let rtx_5090_real = reading(
            HostFacts {
                total_memory_bytes: 64 * GIB,
                available_memory_bytes: 60 * GIB,
                ..Default::default()
            },
            RTX_5090_ID,
            AcceleratorFacts {
                model: "NVIDIA GeForce RTX 5090".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Dedicated as i32,
                total_memory_bytes: Some(32_607 * MIB),
                used_memory_bytes: Some(497 * MIB + 10 * GIB),
                driver_or_runtime_version: "595.71.05".to_owned(),
                ..Default::default()
            },
        );
        let rtx_5090_declared = declared(
            "rtx-5090",
            MachineWorkload {
                gpu_bytes: [(RTX_5090_ID.to_owned(), 10 * GIB)].into(),
                host_bytes: 0,
            },
        );
        let rtx_5090 = project(rtx_5090_declared);
        assert_eq!(rtx_5090, project(rtx_5090_real));
        assert_eq!(
            rtx_5090.accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Dedicated)
        );
        assert_eq!(
            rtx_5090.memory,
            CapabilityObservation::Available(NodeMemoryFacts {
                devices: vec![DeviceMemoryFacts {
                    device_id: RTX_5090_ID.to_owned(),
                    model: "NVIDIA GeForce RTX 5090".to_owned(),
                    vendor_index: Some(0),
                    total_bytes: Some(32_607 * MIB),
                    used_bytes: Some(497 * MIB + 10 * GIB),
                }],
                host: Some(HostMemoryFacts {
                    total_bytes: 64 * GIB,
                    available_bytes: 60 * GIB,
                }),
            })
        );

        // A 20 GiB workload on the GB10, which reads as used host memory.
        let gb10_real = reading(
            HostFacts {
                total_memory_bytes: GB10_HOST_TOTAL,
                available_memory_bytes: GB10_HOST_TOTAL - GB10_IDLE_USED - 20 * GIB,
                ..Default::default()
            },
            GB10_ID,
            AcceleratorFacts {
                model: "NVIDIA GB10".to_owned(),
                memory_architecture: AcceleratorMemoryArchitecture::Unified as i32,
                driver_or_runtime_version: "580.173.02".to_owned(),
                ..Default::default()
            },
        );
        let gb10_declared = declared(
            "gb10",
            MachineWorkload {
                gpu_bytes: [(GB10_ID.to_owned(), 20 * GIB)].into(),
                host_bytes: 0,
            },
        );
        let gb10 = project(gb10_declared);
        assert_eq!(gb10, project(gb10_real));
        assert_eq!(
            gb10.accelerator_memory,
            CapabilityObservation::Available(MemoryArchitecture::Unified)
        );
        assert_eq!(
            host_memory(&gb10),
            HostMemoryFacts {
                total_bytes: GB10_HOST_TOTAL,
                available_bytes: GB10_HOST_TOTAL - GB10_IDLE_USED - 20 * GIB,
            }
        );
        assert!(matches!(
            &gb10.nvidia_driver,
            CapabilityObservation::Available(driver) if driver.major() == 580
        ));
    }

    #[test]
    fn failed_collection_never_restamps_earlier_facts() {
        let now = SystemTime::now();
        let feed = NodeObservationFeed::default();
        let source = NodeTargetCapabilitySource::new(
            &[target()],
            feed.clone(),
            TARGET_CAPABILITY_MAX_AGE,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        );
        feed.publish(
            crate::node::agent::NodeCollection::without_processes(available_snapshot()),
            1,
            now - Duration::from_secs(10),
        );
        assert_eq!(
            capabilities_at(&source, now).platform,
            CapabilityObservation::Available(ExecutionPlatform::linux_amd64())
        );

        feed.publish(
            crate::node::agent::NodeCollection::without_processes(failed_snapshot()),
            2,
            now,
        );

        let capabilities = capabilities_at(&source, now);
        assert_eq!(capabilities.platform, CapabilityObservation::Unavailable);
        assert_eq!(
            capabilities.nvidia_driver,
            crate::node::target_inventory::CapabilityObservation::Unavailable
        );
        assert_eq!(capabilities.accelerator_observed_at, None);
    }

    #[test]
    fn injected_max_age_marks_an_older_collection_stale() {
        let now = SystemTime::now();
        let feed = NodeObservationFeed::default();
        let source = NodeTargetCapabilitySource::new(
            &[target()],
            feed.clone(),
            Duration::from_secs(1),
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        );
        feed.publish(
            crate::node::agent::NodeCollection::without_processes(available_snapshot()),
            1,
            now - Duration::from_secs(2),
        );

        assert_eq!(
            capabilities_at(&source, now).platform,
            CapabilityObservation::Stale(ExecutionPlatform::linux_amd64())
        );
    }

    #[test]
    fn missing_collection_is_an_observation_failure() {
        let source = NodeTargetCapabilitySource::new(
            &[target()],
            NodeObservationFeed::default(),
            TARGET_CAPABILITY_MAX_AGE,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        );

        assert!(source.inventory_at(SystemTime::now()).is_err());
    }

    fn capabilities_at(source: &NodeTargetCapabilitySource, now: SystemTime) -> TargetCapabilities {
        source
            .inventory_at(now)
            .expect("inventory")
            .for_target("target-one")
            .expect("target capabilities")
            .clone()
    }

    /// Host memory facts of a target whose node reported them.
    fn host_memory(capabilities: &TargetCapabilities) -> HostMemoryFacts {
        match &capabilities.memory {
            CapabilityObservation::Available(memory) => memory.host.expect("host memory"),
            other => panic!("memory facts unavailable: {other:?}"),
        }
    }

    fn target() -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "target-one".to_owned(),
            site_node_id: "worker-one".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                ..Default::default()
            }),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                        DockerRuntimeAdapter::default(),
                    )),
                },
            )),
            ..Default::default()
        }
    }

    fn available_snapshot() -> NodeObservationSnapshot {
        NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Snapshot(
                    HardwareSnapshot {
                        accelerators: vec![AcceleratorObservation {
                            vendor: AcceleratorVendor::Nvidia as i32,
                            result: Some(accelerator_observation::Result::Facts(
                                AcceleratorFactsSet {
                                    facts: vec![AcceleratorFacts {
                                        vendor: AcceleratorVendor::Nvidia as i32,
                                        driver_or_runtime_version: "580.173.02".to_owned(),
                                        memory_architecture:
                                            AcceleratorMemoryArchitecture::Dedicated as i32,
                                        ..Default::default()
                                    }],
                                },
                            )),
                        }],
                        ..Default::default()
                    },
                )),
            }),
            runtimes: vec![RuntimeObservation {
                execution_target_id: "target-one".to_owned(),
                result: Some(runtime_observation::Result::Available(
                    dotcore_proto::google::protobuf::Empty {},
                )),
                ..Default::default()
            }],
        }
    }

    fn failed_snapshot() -> NodeObservationSnapshot {
        let unavailable = || ObservationUnavailable {
            code: "COLLECTION_FAILED".to_owned(),
            message: "collection failed".to_owned(),
        };
        NodeObservationSnapshot {
            hardware: Some(NodeHardwareObservation {
                result: Some(node_hardware_observation::Result::Unavailable(unavailable())),
            }),
            runtimes: vec![RuntimeObservation {
                execution_target_id: "target-one".to_owned(),
                result: Some(runtime_observation::Result::Unavailable(unavailable())),
                ..Default::default()
            }],
        }
    }
}
