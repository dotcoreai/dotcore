//! The capacity component's node memory read: every site node's memory pools with what occupies
//! them, as site memory views draw them.
//!
//! It joins the site manager's latest memory reading of each node with what every lane's
//! deployments hold there, and answers in the API's terms. Both are read from memory without
//! serializing with lifecycle operations, so the read is cheap enough to watch and is corrected by
//! the next one. Unchanged state answers the same bytes; only each report's receipt time moves.

mod conversion;
mod service;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use dotcore_proto::dotcore::capacity::v1::ListNodeMemoryResponse;
use dotcore_site::node::{
    registry::NodeRegistry,
    target_inventory::{
        NodeMemoryObservation, TargetCapabilityInventory, TargetCapabilitySourceError,
    },
};
use tracing::{debug, info, warn};

use crate::hardware::workloads::{SiteWorkloadSource, SiteWorkloadsError};
pub(crate) use conversion::memory_pool;
use conversion::node_memory;
pub(crate) use service::CapacityService;

/// Reads every site node's memory pools with what each lane's deployments hold there.
pub(crate) struct NodeMemoryRead {
    readings: Arc<dyn NodeReadings>,
    workloads: Arc<dyn SiteWorkloadSource>,

    /// Whether the last read found target observations unreadable.
    observations_unreadable: AtomicBool,
}

impl NodeMemoryRead {
    /// Creates the read over the nodes' memory `readings` and the site's `workloads`.
    pub(crate) fn new(
        readings: Arc<dyn NodeReadings>,
        workloads: Arc<dyn SiteWorkloadSource>,
    ) -> Self {
        Self {
            readings,
            workloads,
            observations_unreadable: AtomicBool::new(false),
        }
    }

    /// Lists every site node's memory, in site order. A node without a reading is listed without
    /// pools. When target observations can't be read, deployments' devices don't resolve: each
    /// keeps the use attributed to it on a device, and device pools list it as unsized where it
    /// holds nothing, rather than guess.
    pub(crate) fn list(&self) -> Result<ListNodeMemoryResponse, NodeMemoryReadError> {
        let readings = self
            .readings
            .node_memory()
            .map_err(NodeMemoryReadError::Readings)?;
        // A watch reads every tick, so a lasting failure warns once and its recovery is noted.
        let inventory = match self.readings.target_capabilities() {
            Ok(inventory) => {
                if self.observations_unreadable.swap(false, Ordering::Relaxed) {
                    info!("target observations are readable again");
                }
                Some(inventory)
            }
            Err(error) => {
                if self.observations_unreadable.swap(true, Ordering::Relaxed) {
                    debug!(error = %error, "target observations are still unavailable");
                } else {
                    warn!(
                        error = %error,
                        "target observations are unavailable; node memory leaves deployment devices unresolved"
                    );
                }
                None
            }
        };
        let workloads = self
            .workloads
            .node_workloads(inventory.as_ref())
            .map_err(NodeMemoryReadError::Workloads)?;
        Ok(ListNodeMemoryResponse {
            nodes: readings
                .iter()
                .map(|observation| {
                    node_memory(
                        observation,
                        workloads
                            .get(&observation.site_node_id)
                            .map_or(&[], Vec::as_slice),
                    )
                })
                .collect(),
        })
    }
}

/// Failures of the node memory read.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NodeMemoryReadError {
    #[error("node memory readings are unavailable")]
    Readings(#[source] TargetCapabilitySourceError),

    #[error("site workloads are unavailable")]
    Workloads(#[source] SiteWorkloadsError),
}

/// The site manager's latest readings of every node: each node's memory and each execution
/// target's capabilities.
pub(crate) trait NodeReadings: Send + Sync {
    /// Every site node's memory reading, in site order.
    fn node_memory(&self) -> Result<Vec<NodeMemoryObservation>, TargetCapabilitySourceError>;

    /// Every execution target's capabilities.
    fn target_capabilities(&self)
    -> Result<TargetCapabilityInventory, TargetCapabilitySourceError>;
}

impl NodeReadings for NodeRegistry {
    fn node_memory(&self) -> Result<Vec<NodeMemoryObservation>, TargetCapabilitySourceError> {
        NodeRegistry::node_memory(self)
    }

    fn target_capabilities(
        &self,
    ) -> Result<TargetCapabilityInventory, TargetCapabilitySourceError> {
        NodeRegistry::target_capabilities(self)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, UNIX_EPOCH},
    };

    use dotcore_domain::test_support::tracing::{Record, capture_records};
    use dotcore_node_hardware::{MachineDescription, MachineWorkload};
    use dotcore_proto::{
        dotcore::{
            application::v1::{ApplicationDeployment, ApplicationServiceAcceleratorMemory},
            capacity::v1::{
                MemoryOwner, MemoryPool, MemoryPoolKind, MemorySegment, MemorySegmentSource,
                NodeMemory, NodeMemoryPool, NodeMemoryState, memory_owner, memory_segment,
            },
            deployment::v1::{
                AcceleratorMemoryDeviceUsage, AcceleratorMemoryObservation,
                AcceleratorMemoryUnavailable, AcceleratorMemoryUsageSet, ExactPlacement,
                accelerator_memory_observation,
            },
            model::v1::{
                ModelDeployment, ModelDeploymentIntent, ModelDeploymentSnapshot,
                ModelDeploymentState, ModelDeploymentStatus, ResolvedLocalModelDeploymentSnapshot,
                ResolvedSelectedTarget, ResolvedSizing, model_deployment_snapshot,
            },
            observation::v1::AcceleratorMemoryArchitecture,
        },
        google::protobuf::Timestamp,
    };
    use dotcore_site::node::target_inventory::{
        NodeAcceleratorObservation, NodeMemoryObservation, TargetCapabilityInventory,
        TargetCapabilityReconciler, TargetCapabilitySourceError,
    };
    use prost::Message;

    use super::{NodeMemoryRead, NodeReadings};
    use crate::hardware::{
        capacity::NodeWorkloads,
        workloads::{SiteWorkloadSource, SiteWorkloadsError, join_lanes},
    };

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const RTX_5090_TOTAL: u64 = 32_607 * MIB;
    const RESERVED: u64 = 497 * MIB;
    const GPU: &str = "GPU-00000000-0000-0000-0000-000000005090";
    const OBSERVED_SECONDS: u64 = 1_725_000_000;

    #[test]
    fn nodes_are_listed_in_site_order_each_with_its_own_deployments() {
        let read = read(
            vec![
                NodeMemoryObservation::unavailable("manager-node"),
                reading("worker-node", &[on_gpu(10 * GIB)]),
                reading("other-node", &[]),
            ],
            vec![model(
                "model",
                ModelDeploymentStatus::Ready,
                "model-target",
                Some(10 * GIB),
            )],
            Vec::new(),
        );

        assert_eq!(
            read.list().expect("node memory").nodes,
            vec![
                NodeMemory {
                    site_node_id: "manager-node".to_owned(),
                    state: NodeMemoryState::Unavailable as i32,
                    observed_at: None,
                    pools: Vec::new(),
                },
                // The ready model's host estimate stands for its host use.
                rtx_5090_node(
                    "worker-node",
                    vec![measured_model("model", 10 * GIB), other(RESERVED)],
                    Vec::new(),
                    vec![estimated_model("model", GIB), other(3 * GIB)],
                ),
                rtx_5090_node(
                    "other-node",
                    vec![other(RESERVED)],
                    Vec::new(),
                    vec![other(4 * GIB)],
                ),
            ]
        );
    }

    #[test]
    fn a_model_whose_target_is_gone_is_unsized_and_an_application_without_architecture_is_other() {
        let read = read(
            vec![reading("worker-node", &[on_gpu(2 * GIB)])],
            vec![model(
                "removed",
                ModelDeploymentStatus::Loading,
                "removed-target",
                None,
            )],
            vec![application(
                "orphan",
                "removed-app-target",
                vec![Some(available(2 * GIB))],
            )],
        );

        // The starting model's devices don't resolve, so its claim on the GPU is unknown and the
        // GPU can't draw it; its host estimate stays claimed.
        assert_eq!(
            read.list().expect("node memory").nodes,
            vec![rtx_5090_node(
                "worker-node",
                vec![other(2 * GIB + RESERVED)],
                vec![model_owner("removed")],
                vec![
                    MemorySegment {
                        source: MemorySegmentSource::Claimed as i32,
                        owner: Some(memory_segment::Owner::ModelDeploymentId(
                            "removed".to_owned()
                        )),
                        bytes: GIB,
                    },
                    other(4 * GIB),
                ],
            )]
        );
    }

    #[test]
    fn applications_follow_the_models_with_their_units_summed_and_inapplicable_ones_never_unsized()
    {
        let read = read(
            vec![reading(
                "worker-node",
                &[on_gpu(10 * GIB), on_gpu(GIB), on_gpu(2 * GIB)],
            )],
            vec![model(
                "model",
                ModelDeploymentStatus::Ready,
                "model-target",
                Some(10 * GIB),
            )],
            vec![
                application(
                    "multi",
                    "app-target",
                    vec![Some(available(GIB)), Some(available(2 * GIB))],
                ),
                application(
                    "cpu",
                    "app-target",
                    vec![Some(accelerator_memory_observation::Result::NotApplicable(
                        Default::default(),
                    ))],
                ),
                application(
                    "unread",
                    "app-target",
                    vec![Some(accelerator_memory_observation::Result::Unavailable(
                        AcceleratorMemoryUnavailable::default(),
                    ))],
                ),
            ],
        );

        assert_eq!(
            read.list().expect("node memory").nodes,
            vec![rtx_5090_node(
                "worker-node",
                vec![
                    measured_model("model", 10 * GIB),
                    MemorySegment {
                        source: MemorySegmentSource::Measured as i32,
                        owner: Some(memory_segment::Owner::ApplicationDeploymentId(
                            "multi".to_owned()
                        )),
                        bytes: 3 * GIB,
                    },
                    other(RESERVED),
                ],
                vec![MemoryOwner {
                    owner: Some(memory_owner::Owner::ApplicationDeploymentId(
                        "unread".to_owned()
                    )),
                }],
                vec![estimated_model("model", GIB), other(3 * GIB)],
            )]
        );
    }

    #[test]
    fn unreadable_target_observations_leave_every_deployment_unresolved_but_listed() {
        let (read, readings) = read_with_readings(
            vec![reading("worker-node", &[on_gpu(10 * GIB), on_gpu(2 * GIB)])],
            vec![model(
                "model",
                ModelDeploymentStatus::Ready,
                "model-target",
                Some(10 * GIB),
            )],
            vec![
                application("measured", "app-target", vec![Some(available(2 * GIB))]),
                application(
                    "unread",
                    "app-target",
                    vec![Some(accelerator_memory_observation::Result::Unavailable(
                        AcceleratorMemoryUnavailable::default(),
                    ))],
                ),
            ],
        );
        *readings.inventory.lock().expect("inventory") = None;

        // Attributed use still lands on its GPU; an application without a reading is unsized
        // on every GPU it may use.
        assert_eq!(
            read.list().expect("node memory").nodes,
            vec![rtx_5090_node(
                "worker-node",
                vec![
                    measured_model("model", 10 * GIB),
                    MemorySegment {
                        source: MemorySegmentSource::Measured as i32,
                        owner: Some(memory_segment::Owner::ApplicationDeploymentId(
                            "measured".to_owned()
                        )),
                        bytes: 2 * GIB,
                    },
                    other(RESERVED),
                ],
                vec![MemoryOwner {
                    owner: Some(memory_owner::Owner::ApplicationDeploymentId(
                        "unread".to_owned()
                    )),
                }],
                vec![estimated_model("model", GIB), other(3 * GIB)],
            )]
        );
    }

    #[test]
    fn unreadable_target_observations_warn_once_until_they_recover() {
        let (read, readings) = read_with_readings(Vec::new(), Vec::new(), Vec::new());
        let inventory = readings.inventory.lock().expect("inventory").clone();
        let set = |inventory: Option<TargetCapabilityInventory>| {
            *readings.inventory.lock().expect("inventory") = inventory;
        };

        let (_, records, _) = capture_records(|| {
            set(None);
            for _ in 0..3 {
                read.list().expect("node memory");
            }
            set(inventory.clone());
            read.list().expect("node memory");
            read.list().expect("node memory");
            set(None);
            read.list().expect("node memory");
        });

        let unavailable =
            "target observations are unavailable; node memory leaves deployment devices unresolved";
        let readable = "target observations are readable again";
        assert_eq!(
            records
                .iter()
                .filter_map(|record| match record {
                    Record::Event { level, fields } if level == "WARN" || level == "INFO" =>
                        Some((level.as_str(), fields["message"].as_str())),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec![
                ("WARN", unavailable),
                ("INFO", readable),
                ("WARN", unavailable)
            ]
        );
    }

    #[test]
    fn two_reads_of_unchanged_state_encode_to_the_same_bytes() {
        let read = read(
            vec![
                NodeMemoryObservation::unavailable("manager-node"),
                reading("worker-node", &[on_gpu(10 * GIB), on_gpu(GIB)]),
            ],
            vec![model(
                "model",
                ModelDeploymentStatus::Loading,
                "model-target",
                Some(4 * GIB),
            )],
            vec![application(
                "multi",
                "app-target",
                vec![Some(available(GIB))],
            )],
        );

        let first = read.list().expect("node memory").encode_to_vec();
        let second = read.list().expect("node memory").encode_to_vec();
        assert_eq!(first, second);
    }

    /// A node memory read over fixed `readings` and fixed deployments of both lanes, whose targets
    /// are on the worker node's GPU.
    fn read(
        readings: Vec<NodeMemoryObservation>,
        models: Vec<ModelDeployment>,
        applications: Vec<ApplicationDeployment>,
    ) -> NodeMemoryRead {
        read_with_readings(readings, models, applications).0
    }

    /// [`read`] with the readings it reads, whose target observations a test can make fail.
    fn read_with_readings(
        readings: Vec<NodeMemoryObservation>,
        models: Vec<ModelDeployment>,
        applications: Vec<ApplicationDeployment>,
    ) -> (NodeMemoryRead, Arc<FixedReadings>) {
        let snapshot = render(&[]);
        let inventory = TargetCapabilityReconciler::reconcile_now(
            [
                ("model-target", "worker-node", [GPU]),
                ("app-target", "worker-node", [GPU]),
            ],
            [],
            [NodeAcceleratorObservation::from_hardware_snapshot(
                "worker-node",
                &snapshot,
                false,
            )],
        );
        let readings = Arc::new(FixedReadings {
            memory: readings,
            inventory: Mutex::new(Some(inventory)),
        });
        let read = NodeMemoryRead::new(
            readings.clone(),
            Arc::new(FixedWorkloads {
                models,
                applications,
            }),
        );
        (read, readings)
    }

    /// The fresh reading of an `rtx-5090` node running `running`.
    fn reading(node: &str, running: &[MachineWorkload]) -> NodeMemoryObservation {
        NodeMemoryObservation::from_hardware_snapshot(node, &render(running), false)
    }

    fn render(
        running: &[MachineWorkload],
    ) -> dotcore_proto::dotcore::observation::v1::HardwareSnapshot {
        MachineDescription::simulated(&["rtx-5090".to_owned()], "amd64")
            .expect("named machine")
            .render(running, UNIX_EPOCH + Duration::from_secs(OBSERVED_SECONDS))
            .expect("rendered snapshot")
    }

    fn on_gpu(bytes: u64) -> MachineWorkload {
        MachineWorkload {
            gpu_bytes: [(GPU.to_owned(), bytes)].into(),
            host_bytes: 0,
        }
    }

    /// An available `rtx-5090` node drawing `gpu` segments, with `gpu_unsized`
    /// deployments on its GPU, and `host` segments.
    fn rtx_5090_node(
        node: &str,
        gpu: Vec<MemorySegment>,
        gpu_unsized: Vec<MemoryOwner>,
        host: Vec<MemorySegment>,
    ) -> NodeMemory {
        NodeMemory {
            site_node_id: node.to_owned(),
            state: NodeMemoryState::Available as i32,
            observed_at: Some(Timestamp {
                seconds: OBSERVED_SECONDS as i64,
                nanos: 0,
            }),
            pools: vec![
                NodeMemoryPool {
                    pool: Some(MemoryPool {
                        kind: MemoryPoolKind::Device as i32,
                        device_id: GPU.to_owned(),
                        device_name: "NVIDIA GeForce RTX 5090".to_owned(),
                        device_index: Some(0),
                        total_bytes: Some(RTX_5090_TOTAL),
                        segments: gpu,
                        over_total_bytes: 0,
                    }),
                    r#unsized: gpu_unsized,
                },
                NodeMemoryPool {
                    pool: Some(MemoryPool {
                        kind: MemoryPoolKind::Host as i32,
                        device_id: String::new(),
                        device_name: String::new(),
                        device_index: None,
                        total_bytes: Some(64 * GIB),
                        segments: host,
                        over_total_bytes: 0,
                    }),
                    r#unsized: Vec::new(),
                },
            ],
        }
    }

    fn measured_model(id: &str, bytes: u64) -> MemorySegment {
        MemorySegment {
            source: MemorySegmentSource::Measured as i32,
            owner: Some(memory_segment::Owner::ModelDeploymentId(id.to_owned())),
            bytes,
        }
    }

    fn estimated_model(id: &str, bytes: u64) -> MemorySegment {
        MemorySegment {
            source: MemorySegmentSource::Estimated as i32,
            owner: Some(memory_segment::Owner::ModelDeploymentId(id.to_owned())),
            bytes,
        }
    }

    fn other(bytes: u64) -> MemorySegment {
        MemorySegment {
            source: MemorySegmentSource::Other as i32,
            owner: None,
            bytes,
        }
    }

    fn model_owner(id: &str) -> MemoryOwner {
        MemoryOwner {
            owner: Some(memory_owner::Owner::ModelDeploymentId(id.to_owned())),
        }
    }

    /// A local model deployment on the worker node's `target`, estimated at 10 GiB of GPU memory
    /// and 1 GiB of host memory, with `attributed` GPU memory.
    fn model(
        id: &str,
        status: ModelDeploymentStatus,
        target: &str,
        attributed: Option<u64>,
    ) -> ModelDeployment {
        ModelDeployment {
            model_deployment_id: id.to_owned(),
            intent: Some(ModelDeploymentIntent {
                placement: Some(placement(target)),
                ..Default::default()
            }),
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(model_deployment_snapshot::Source::Local(
                    ResolvedLocalModelDeploymentSnapshot {
                        selected_target: Some(ResolvedSelectedTarget {
                            accelerator_memory_architecture:
                                AcceleratorMemoryArchitecture::Dedicated as i32,
                            ..Default::default()
                        }),
                        sizing: Some(ResolvedSizing {
                            accelerator_memory_bytes: Some(10 * GIB),
                            host_memory_bytes: Some(GIB),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )),
            }),
            state: Some(ModelDeploymentState {
                status: status as i32,
                ..Default::default()
            }),
            accelerator_memory: attributed.map(|bytes| AcceleratorMemoryObservation {
                result: Some(available(bytes)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// An application deployment on the worker node's `target` whose units report `units`.
    fn application(
        id: &str,
        target: &str,
        units: Vec<Option<accelerator_memory_observation::Result>>,
    ) -> ApplicationDeployment {
        ApplicationDeployment {
            application_deployment_id: id.to_owned(),
            placement: Some(placement(target)),
            service_accelerator_memory: units
                .into_iter()
                .enumerate()
                .map(|(index, result)| ApplicationServiceAcceleratorMemory {
                    service_name: format!("unit-{index}"),
                    accelerator_memory: Some(AcceleratorMemoryObservation {
                        result,
                        ..Default::default()
                    }),
                })
                .collect(),
            ..Default::default()
        }
    }

    fn available(bytes: u64) -> accelerator_memory_observation::Result {
        accelerator_memory_observation::Result::Available(AcceleratorMemoryUsageSet {
            devices: vec![AcceleratorMemoryDeviceUsage {
                managed_node_id: "worker-node".to_owned(),
                device_id: GPU.to_owned(),
                used_memory_bytes: bytes,
            }],
        })
    }

    fn placement(target: &str) -> ExactPlacement {
        ExactPlacement {
            site_node_id: "worker-node".to_owned(),
            execution_target_id: target.to_owned(),
        }
    }

    /// Fixed node readings; target observations fail while there is no inventory.
    struct FixedReadings {
        memory: Vec<NodeMemoryObservation>,
        inventory: Mutex<Option<TargetCapabilityInventory>>,
    }

    impl NodeReadings for FixedReadings {
        fn node_memory(&self) -> Result<Vec<NodeMemoryObservation>, TargetCapabilitySourceError> {
            Ok(self.memory.clone())
        }

        fn target_capabilities(
            &self,
        ) -> Result<TargetCapabilityInventory, TargetCapabilitySourceError> {
            self.inventory
                .lock()
                .expect("inventory")
                .clone()
                .ok_or_else(|| {
                    TargetCapabilitySourceError::Observation(anyhow::anyhow!("observations failed"))
                })
        }
    }

    /// Fixed deployments of both lanes, answered through each lane's builder.
    struct FixedWorkloads {
        models: Vec<ModelDeployment>,
        applications: Vec<ApplicationDeployment>,
    }

    impl SiteWorkloadSource for FixedWorkloads {
        fn node_workloads(
            &self,
            inventory: Option<&TargetCapabilityInventory>,
        ) -> Result<NodeWorkloads, SiteWorkloadsError> {
            Ok(join_lanes([
                crate::model::compatibility::node_workloads(&self.models, inventory),
                crate::application::workloads::node_workloads(&self.applications, inventory),
            ]))
        }
    }
}
