//! Model service projection of desired deployments and current state.

use std::{
    collections::BTreeMap,
    sync::{Arc, Weak},
};

use dotcore_proto::dotcore::{
    deployment::v1::AcceleratorMemoryObservation,
    model::v1::{
        ListModelDeploymentsResponse, ModelDeployment, ModelDeploymentDeclaration,
        ModelDeploymentIntent, ModelDeploymentState, ModelDeploymentStatus,
        model_deployment_snapshot::Source as ModelDeploymentSource,
    },
    site::v1::ExecutionTarget,
};
use dotcore_site::{
    node::control::{
        DeploymentStatusEvidence, ManagerNodeControlCoordinator, ModelStatusEvidence,
        NodeDeploymentReference,
    },
    repository::ModelSiteSnapshot,
};

use super::{
    CurrentStateRepository, ModelLifecycleError,
    accelerator_memory::node_hosted_projection,
    state::{configuration, current_error, desired_error, desired_state},
};

/// Reads an inference controller's projected deployments as the list read does, without waiting
/// for a lifecycle operation and without keeping the controller alive, so a source the controller
/// itself holds can read it. Once the controller is gone, it reads no deployment.
#[derive(Clone)]
pub(crate) struct DeploymentProjection(Weak<dyn ProjectsDeployments>);

impl DeploymentProjection {
    /// Projects the controller's deployments; a deployment being created or removed may be missing
    /// or still listed until the next read.
    pub(crate) fn deployments(&self) -> Result<Vec<ModelDeployment>, ModelLifecycleError> {
        match self.0.upgrade() {
            Some(controller) => controller.deployments(),
            None => Ok(Vec::new()),
        }
    }
}

#[expect(
    private_bounds,
    reason = "the public selected controller alias retains private owner-specific repository ports"
)]
impl<D, S> super::InferenceController<D, S>
where
    D: super::DesiredDeploymentRepository + 'static,
    S: CurrentStateRepository + 'static,
{
    /// Returns a reader of this controller's projected deployments that doesn't keep it alive.
    pub(crate) fn deployment_projection(&self) -> DeploymentProjection {
        let inner: Weak<super::ManagerInferenceControllerInner<D, S>> = Arc::downgrade(&self.inner);
        DeploymentProjection(inner)
    }
}

/// A controller whose deployments a [`DeploymentProjection`] reads.
trait ProjectsDeployments: Send + Sync {
    fn deployments(&self) -> Result<Vec<ModelDeployment>, ModelLifecycleError>;
}

impl<D, S> ProjectsDeployments for super::ManagerInferenceControllerInner<D, S>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    fn deployments(&self) -> Result<Vec<ModelDeployment>, ModelLifecycleError> {
        Ok(self.public_projection()?.deployments)
    }
}

impl<D, S> super::ManagerInferenceControllerInner<D, S>
where
    D: super::DesiredDeploymentRepository,
    S: CurrentStateRepository,
{
    /// Projects one deployment with the given state, as a lifecycle operation answers it.
    pub(super) fn project_deployment(
        &self,
        desired: &ModelDeploymentDeclaration,
        state: ModelDeploymentState,
    ) -> Result<ModelDeployment, ModelLifecycleError> {
        let site = self.desired.declaration_snapshot().map_err(desired_error)?;
        let remote_status = self.remote_status();
        let evidence = node_status_evidence(std::slice::from_ref(desired), remote_status.as_ref());
        let mut deployment = desired_deployment(desired, state.clone())?;
        deployment.accelerator_memory = accelerator_memory(
            desired,
            &state,
            placed_target(&site.execution_targets, desired),
            evidence.get(desired.model_deployment_id.as_str()),
        );
        Ok(deployment)
    }

    pub(super) fn public_projection(
        &self,
    ) -> Result<ListModelDeploymentsResponse, ModelLifecycleError> {
        let desired = self.desired.declaration_snapshot().map_err(desired_error)?;
        let current = self.state.snapshot().map_err(current_error)?;
        project_snapshot(
            &desired,
            current.model_deployments,
            self.remote_status().as_ref(),
        )
    }

    fn remote_status(&self) -> Option<ManagerNodeControlCoordinator> {
        self.remote_status
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

fn project_snapshot(
    desired_snapshot: &ModelSiteSnapshot,
    current: Vec<dotcore_proto::dotcore::model::v1::CurrentModelDeployment>,
    remote_status: Option<&ManagerNodeControlCoordinator>,
) -> Result<ListModelDeploymentsResponse, ModelLifecycleError> {
    let records = current
        .into_iter()
        .map(|value| (value.model_deployment_id.clone(), value))
        .collect::<BTreeMap<_, _>>();
    let node_evidence = node_status_evidence(&desired_snapshot.deployments, remote_status);
    let deployments = desired_snapshot
        .deployments
        .iter()
        .map(|desired| {
            let record = records.get(&desired.model_deployment_id);
            let recorded = record.and_then(|record| record.state.clone());
            let node = node_evidence.get(desired.model_deployment_id.as_str());
            let state = match node {
                // An upstream deployment has no hosting node; its reconciler records its state.
                None => recorded.ok_or_else(|| {
                    configuration(format!(
                        "desired deployment {:?} has no current-state record",
                        desired.model_deployment_id
                    ))
                })?,
                Some(node) => {
                    let trusted = match node.evidence {
                        DeploymentStatusEvidence::Current => true,
                        // Only the record an accepted mutation wrote stands in for the node's
                        // acknowledgement; a record from an earlier report is not evidence.
                        DeploymentStatusEvidence::AcknowledgementPending => record
                            .is_some_and(|record| record.reconciled_lifecycle_generation.is_none()),
                        DeploymentStatusEvidence::Unavailable => false,
                    };
                    match recorded {
                        Some(state) if trusted => state,
                        recorded => super::state::unavailable_state(recorded.as_ref()),
                    }
                }
            };
            let mut deployment = desired_deployment(desired, state.clone())?;
            deployment.accelerator_memory = accelerator_memory(
                desired,
                &state,
                placed_target(&desired_snapshot.execution_targets, desired),
                node,
            );
            Ok::<_, ModelLifecycleError>(deployment)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ListModelDeploymentsResponse { deployments })
}

/// Projects one deployment's accelerator memory as its hosting node reported it.
fn accelerator_memory(
    desired: &ModelDeploymentDeclaration,
    state: &ModelDeploymentState,
    target: Option<&ExecutionTarget>,
    node: Option<&ModelStatusEvidence>,
) -> Option<AcceleratorMemoryObservation> {
    Some(node_hosted_projection(
        desired,
        state,
        target,
        node.and_then(|node| node.accelerator_memory.clone()),
    ))
}

fn placed_target<'a>(
    targets: &'a [ExecutionTarget],
    desired: &ModelDeploymentDeclaration,
) -> Option<&'a ExecutionTarget> {
    targets.iter().find(|target| {
        desired
            .placement
            .as_ref()
            .is_some_and(|placement| target.execution_target_id == placement.execution_target_id)
    })
}

/// Asks the coordinator once for the status evidence of every node-hosted deployment, keyed by
/// deployment ID. Without a coordinator no node evidence exists, so each such deployment is
/// Unavailable. Upstream deployments have no placement and no entry.
fn node_status_evidence<'a>(
    deployments: &'a [ModelDeploymentDeclaration],
    remote_status: Option<&ManagerNodeControlCoordinator>,
) -> BTreeMap<&'a str, ModelStatusEvidence> {
    let hosted = deployments
        .iter()
        .filter_map(|desired| {
            let placement = desired.placement.as_ref()?;
            Some(NodeDeploymentReference {
                node_id: &placement.site_node_id,
                deployment_id: &desired.model_deployment_id,
                lifecycle_generation: desired.lifecycle_generation,
            })
        })
        .collect::<Vec<_>>();
    let evidence = match remote_status {
        Some(coordinator) if !hosted.is_empty() => coordinator.model_status_evidence(&hosted),
        _ => vec![
            ModelStatusEvidence {
                evidence: DeploymentStatusEvidence::Unavailable,
                accelerator_memory: None,
            };
            hosted.len()
        ],
    };
    hosted
        .iter()
        .map(|deployment| deployment.deployment_id)
        .zip(evidence)
        .collect()
}

/// Projects controller-owned desired intent and current state for model service reads.
pub(super) fn desired_deployment(
    desired: &ModelDeploymentDeclaration,
    state: ModelDeploymentState,
) -> Result<ModelDeployment, ModelLifecycleError> {
    let desired_state = desired_state(desired)?;
    Ok(ModelDeployment {
        model_deployment_id: desired.model_deployment_id.clone(),
        model_deployment_name: desired.model_deployment_name.clone(),
        intent: Some(ModelDeploymentIntent {
            placement: desired.placement.clone(),
            requested_parallel_shape: desired.requested_parallel_shape,
        }),
        snapshot: desired.snapshot.clone(),
        state: Some(sanitize_state(desired_state, state)),
        desired_state: desired_state as i32,
        upstream_desired_configuration: desired.snapshot.as_ref().and_then(|snapshot| {
            matches!(
                snapshot.source.as_ref(),
                Some(ModelDeploymentSource::Upstream(_))
            )
            .then(|| desired.upstream_desired_configuration.clone())
            .flatten()
        }),
        accelerator_memory: None,
        node_resolved_launch_identities: desired.node_resolved_launch_identities.clone(),
    })
}

fn sanitize_state(
    desired_state: dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState,
    state: ModelDeploymentState,
) -> ModelDeploymentState {
    match desired_state {
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Paused
            if !matches!(
                ModelDeploymentStatus::try_from(state.status),
                Ok(ModelDeploymentStatus::Pausing
                    | ModelDeploymentStatus::Paused
                    | ModelDeploymentStatus::Unavailable)
            ) =>
        {
            super::state::pausing_state(Some(&state))
        }
        dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Teardown
            if !matches!(
                ModelDeploymentStatus::try_from(state.status),
                Ok(ModelDeploymentStatus::Unloading | ModelDeploymentStatus::Failed)
            ) =>
        {
            super::state::unloading_state(Some(&state))
        }
        _ => state,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use dotcore_proto::dotcore::{
        deployment::v1::{AcceleratorMemoryObservation, NodeResolvedLaunchIdentity},
        model::{
            private::v1::{
                ModelControllerStatusSection, ModelDeploymentStatus as PrivateModelDeploymentStatus,
            },
            v1::{
                CurrentModelDeployment, ModelDeploymentDeclaration, ModelDeploymentDesiredState,
                ModelDeploymentStatus, UpstreamModelDesiredConfiguration,
            },
        },
        site::{
            private::v1::{
                ControllerStatusSection, NodeControllerStatus, NodeControllerStatusAcceptance,
                NodeObservation, controller_status_section,
            },
            v1::ExecutionTarget,
        },
    };
    use dotcore_site::{
        node::{
            control::{
                AcceptedNodeObservationInput, ManagerNodeControlCoordinator, PublishedViewInput,
            },
            observation_configuration::NodeObservationConfigurationDigest,
        },
        repository::ModelSiteSnapshot,
        view::fencing::{ManagerSession, NodeIncarnation, NodeViewProjection},
    };

    use super::desired_deployment;
    use crate::model::controller::{
        InferenceController,
        test_support::{TestCurrent, TestDesired, current_deployment, test_desired_deployment},
    };

    #[test]
    fn local_deployments_omit_upstream_configuration() {
        let mut desired =
            test_desired_deployment("local-model", ModelDeploymentDesiredState::Present);
        desired.upstream_desired_configuration = Some(UpstreamModelDesiredConfiguration {
            api_key: "must-not-project".to_owned(),
            ..Default::default()
        });
        let state = current_deployment(&desired.model_deployment_id)
            .state
            .expect("current state");

        let deployment = desired_deployment(&desired, state).expect("deployment projection");

        assert_eq!(deployment.upstream_desired_configuration, None);
    }

    #[test]
    fn recorded_engine_pin_projects_beside_the_unchanged_snapshot() {
        let mut desired =
            test_desired_deployment("local-model", ModelDeploymentDesiredState::Present);
        let pin = NodeResolvedLaunchIdentity {
            selection_key: "engine".to_owned(),
            launch_identity: format!("sha256:{}", "a".repeat(64)),
        };
        desired.node_resolved_launch_identities = vec![pin.clone()];
        let state = current_deployment(&desired.model_deployment_id)
            .state
            .expect("current state");

        let deployment = desired_deployment(&desired, state).expect("deployment projection");

        assert_eq!(deployment.node_resolved_launch_identities, [pin]);
        assert_eq!(deployment.snapshot, desired.snapshot);
    }

    #[test]
    fn remote_ready_record_without_current_coordinator_status_projects_unavailable() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let desired = TestDesired::new(calls.clone());
        let mut remote =
            test_desired_deployment("fixture-model", ModelDeploymentDesiredState::Present);
        remote.model_deployment_id = "remote-deployment".to_owned();
        let placement = remote.placement.as_mut().expect("placement");
        placement.site_node_id = "worker-node".to_owned();
        placement.execution_target_id = "worker-target".to_owned();
        {
            let mut site = desired.state.lock().expect("desired state");
            let mut target = site.execution_targets[0].clone();
            target.site_node_id = "worker-node".to_owned();
            target.execution_target_id = "worker-target".to_owned();
            site.execution_targets.push(target);
            site.model_deployments.push(remote);
        }
        let current = TestCurrent::new(calls);
        let mut retained_ready = current_deployment("remote-deployment");
        retained_ready.state.as_mut().expect("state").status = ModelDeploymentStatus::Ready as i32;
        super::CurrentStateRepository::upsert(&current, retained_ready, None)
            .expect("retained remote Ready state");
        let controller = InferenceController::new(
            desired,
            current,
            crate::model::controller::test_support::test_target_capability_source(),
            dotcore_domain::timing::Timing::production().model,
            std::sync::Arc::new(dotcore_domain::clock::SystemClock),
        )
        .expect("model owner");

        let projected = controller
            .inner
            .public_projection()
            .expect("public projection");
        assert_eq!(projected.deployments.len(), 1);
        assert_eq!(
            projected.deployments[0].model_deployment_id,
            "remote-deployment"
        );
        assert_eq!(
            projected.deployments[0]
                .state
                .as_ref()
                .expect("state")
                .status,
            ModelDeploymentStatus::Unavailable as i32
        );
    }

    #[test]
    fn unacknowledged_intent_keeps_its_record_and_leaves_other_node_deployments_current() {
        let snapshot = node_snapshot(vec![
            node_deployment("steady", 1, ModelDeploymentDesiredState::Present),
            node_deployment("pausing", 2, ModelDeploymentDesiredState::Paused),
            node_deployment("resuming", 3, ModelDeploymentDesiredState::Present),
        ]);
        // The node last reported before the pause and the resume were accepted.
        let coordinator = node_coordinator(Some(&[("steady", 1), ("pausing", 1), ("resuming", 2)]));
        let resuming = crate::model::controller::state::resuming_state(3);
        let records = vec![
            node_record("steady", ModelDeploymentStatus::Ready, Some(1)),
            node_record("pausing", ModelDeploymentStatus::Pausing, None),
            CurrentModelDeployment {
                state: Some(resuming),
                ..node_record("resuming", ModelDeploymentStatus::Loading, None)
            },
        ];

        let listed = super::project_snapshot(&snapshot, records, Some(&coordinator))
            .expect("list projection");

        assert_eq!(
            listed_statuses(&listed),
            [
                (ModelDeploymentStatus::Ready, "Ready"),
                (ModelDeploymentStatus::Pausing, "Pausing"),
                (ModelDeploymentStatus::Loading, "Resuming"),
            ]
        );
    }

    #[test]
    fn node_records_without_status_evidence_project_unavailable() {
        let snapshot = node_snapshot(vec![
            node_deployment("pausing", 2, ModelDeploymentDesiredState::Paused),
            node_deployment("ready", 2, ModelDeploymentDesiredState::Present),
        ]);
        let records = || {
            vec![
                node_record("pausing", ModelDeploymentStatus::Pausing, None),
                node_record("ready", ModelDeploymentStatus::Ready, Some(2)),
            ]
        };
        let project = |coordinator| {
            super::project_snapshot(&snapshot, records(), Some(&coordinator))
                .expect("list projection")
        };

        // A Connected node with no accepted status supports neither record.
        assert_eq!(
            listed_statuses(&project(node_coordinator(None))),
            [
                (ModelDeploymentStatus::Unavailable, "Unavailable"),
                (ModelDeploymentStatus::Unavailable, "Unavailable"),
            ]
        );
        // A status for the earlier generations keeps only the intent-written record: a retained
        // Ready record is never shown without a status that lists its generation.
        assert_eq!(
            listed_statuses(&project(node_coordinator(Some(&[
                ("pausing", 1),
                ("ready", 1)
            ])))),
            [
                (ModelDeploymentStatus::Pausing, "Pausing"),
                (ModelDeploymentStatus::Unavailable, "Unavailable"),
            ]
        );
    }

    #[test]
    fn paused_deployment_with_an_unreported_retained_ready_record_projects_unavailable() {
        let deployment = node_deployment("paused", 2, ModelDeploymentDesiredState::Paused);
        let snapshot = node_snapshot(vec![deployment.clone()]);

        // A restarted manager reads a retained Ready record the node never reports for its paused
        // deployment; without an accepted status listing it, the record is fenced.
        let listed = super::project_snapshot(
            &snapshot,
            vec![node_record("paused", ModelDeploymentStatus::Ready, Some(2))],
            Some(&node_coordinator(None)),
        )
        .expect("list projection");

        assert_eq!(
            listed_statuses(&listed),
            [(ModelDeploymentStatus::Unavailable, "Unavailable")]
        );
        let projected = &listed.deployments[0];
        assert_eq!(projected.model_deployment_id, "paused");
        assert_eq!(
            projected.desired_state,
            ModelDeploymentDesiredState::Paused as i32
        );
        assert_eq!(projected.snapshot, deployment.snapshot);
        assert_eq!(
            projected
                .intent
                .as_ref()
                .and_then(|intent| intent.placement.as_ref()),
            deployment.placement.as_ref()
        );
    }

    #[test]
    fn node_reported_downloading_and_warming_up_reach_the_public_projection() {
        use dotcore_proto::dotcore::model::private::v1::{
            ModelBindingReadiness, ModelLifecycleObservation, ModelPreparationProgress,
        };
        let snapshot = node_snapshot(vec![
            node_deployment("downloading", 2, ModelDeploymentDesiredState::Present),
            node_deployment("warming", 2, ModelDeploymentDesiredState::Present),
        ]);
        let reported = |id: &str, lifecycle: ModelLifecycleObservation, message: &str| {
            super::super::remote_status::current_record(&PrivateModelDeploymentStatus {
                model_deployment_id: id.to_owned(),
                lifecycle_generation: 2,
                lifecycle: lifecycle as i32,
                status_message: message.to_owned(),
                preparation_progress: (lifecycle == ModelLifecycleObservation::Downloading)
                    .then_some(ModelPreparationProgress {
                        completed_bytes: 50,
                        total_bytes: Some(100),
                    }),
                binding_readiness: ModelBindingReadiness::NotReady as i32,
                state_revision: 1,
                ..Default::default()
            })
            .expect("node-hosted record")
        };
        let records = vec![
            reported(
                "downloading",
                ModelLifecycleObservation::Downloading,
                "Downloading model artifacts",
            ),
            reported(
                "warming",
                ModelLifecycleObservation::WarmingUp,
                "Warming up",
            ),
        ];

        let projected = super::project_snapshot(
            &snapshot,
            records,
            Some(&node_coordinator(Some(&[
                ("downloading", 2),
                ("warming", 2),
            ]))),
        )
        .expect("list projection");

        assert_eq!(
            listed_statuses(&projected),
            [
                (
                    ModelDeploymentStatus::Downloading,
                    "Downloading model artifacts"
                ),
                (ModelDeploymentStatus::WarmingUp, "Warming up"),
            ]
        );
        assert_eq!(
            projected.deployments[0]
                .state
                .as_ref()
                .and_then(|state| state.progress.as_ref())
                .map(|progress| (progress.completed, progress.total, progress.percent)),
            Some((50, Some(100), Some(50)))
        );
    }

    #[test]
    fn node_hosted_memory_is_the_reported_observation_while_node_status_is_current() {
        use dotcore_proto::dotcore::deployment::v1::{
            AcceleratorMemoryUnavailable, accelerator_memory_observation,
        };
        use dotcore_proto::dotcore::site::v1::{
            DockerRuntimeAdapter, ModelRuntimeBinding, execution_target, model_runtime_binding,
        };

        let mut snapshot = node_snapshot(vec![node_deployment(
            "ready",
            1,
            ModelDeploymentDesiredState::Present,
        )]);
        snapshot.execution_targets[0].accelerator_device_ids = vec!["nvidia.com/gpu=0".to_owned()];
        snapshot.execution_targets[0].runtime_binding = Some(
            execution_target::RuntimeBinding::ModelRuntime(ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::DockerRuntime(
                    DockerRuntimeAdapter::default(),
                )),
            }),
        );
        let records = vec![node_record("ready", ModelDeploymentStatus::Ready, Some(1))];
        let memory = |coordinator: Option<&ManagerNodeControlCoordinator>| {
            super::project_snapshot(&snapshot, records.clone(), coordinator)
                .expect("list projection")
                .deployments[0]
                .accelerator_memory
                .clone()
        };

        assert_eq!(
            memory(Some(&node_coordinator(Some(&[("ready", 1)])))),
            Some(reported_memory())
        );
        // Without current node status, memory reads as unavailable with no refresh state, never
        // as a pending refresh.
        assert_eq!(
            memory(None),
            Some(AcceleratorMemoryObservation {
                observed_at: None,
                refresh: None,
                result: Some(accelerator_memory_observation::Result::Unavailable(
                    AcceleratorMemoryUnavailable {
                        code: "NODE_STATUS_UNAVAILABLE".to_owned(),
                        message:
                            "The hosting node has not reported current status for this deployment."
                                .to_owned(),
                    },
                )),
            })
        );
    }

    fn node_deployment(
        id: &str,
        lifecycle_generation: u64,
        desired_state: ModelDeploymentDesiredState,
    ) -> ModelDeploymentDeclaration {
        let mut deployment = test_desired_deployment("fixture-model", desired_state);
        deployment.model_deployment_id = id.to_owned();
        deployment.lifecycle_generation = lifecycle_generation;
        let placement = deployment.placement.as_mut().expect("placement");
        placement.site_node_id = "worker-node".to_owned();
        placement.execution_target_id = "worker-target".to_owned();
        deployment
    }

    fn node_snapshot(deployments: Vec<ModelDeploymentDeclaration>) -> ModelSiteSnapshot {
        ModelSiteSnapshot {
            site_id: "site-one".to_owned(),
            execution_targets: vec![node_target("worker-target")],
            deployments,
        }
    }

    fn node_target(id: &str) -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: id.to_owned(),
            site_node_id: "worker-node".to_owned(),
            ..Default::default()
        }
    }

    fn node_record(
        id: &str,
        status: ModelDeploymentStatus,
        reconciled_lifecycle_generation: Option<u64>,
    ) -> CurrentModelDeployment {
        let mut record = current_deployment(id);
        record.reconciled_lifecycle_generation = reconciled_lifecycle_generation;
        let state = record.state.as_mut().expect("state");
        state.status = status as i32;
        state.status_message = crate::model::controller::state::status_message(status).to_owned();
        record
    }

    /// Builds a coordinator holding a Connected `worker-node`. Its accepted status, if any, lists
    /// `reported` deployment generations against a view that a later publication has replaced.
    fn node_coordinator(reported: Option<&[(&str, u64)]>) -> ManagerNodeControlCoordinator {
        let session = ManagerSession::parse("a".repeat(32)).expect("manager session");
        let incarnation = NodeIncarnation::parse("1".repeat(32)).expect("incarnation");
        let coordinator = ManagerNodeControlCoordinator::on_system_clock(session.clone());
        coordinator
            .register_observation(
                &NodeObservation {
                    site_id: "site-one".to_owned(),
                    node_id: "worker-node".to_owned(),
                    incarnation_id: incarnation.as_str().to_owned(),
                    revision: 1,
                    observation_configuration_sha256: "0".repeat(64),
                    ..Default::default()
                },
                AcceptedNodeObservationInput {
                    node_id: "worker-node".to_owned(),
                    incarnation: incarnation.clone(),
                    revision: 1,
                    snapshot: Default::default(),
                    observation_configuration_sha256: NodeObservationConfigurationDigest::parse(
                        "0".repeat(64),
                    )
                    .expect("configuration digest"),
                    received_at: Default::default(),
                    private_port: None,
                },
            )
            .expect("registered node");
        let publish = |target_id: &str| {
            coordinator
                .publish_view_for_test(
                    "worker-node",
                    &incarnation,
                    PublishedViewInput {
                        site_id: "site-one".to_owned(),
                        projection: NodeViewProjection::new(
                            vec![node_target(target_id)],
                            Vec::new(),
                        )
                        .expect("node view"),
                    },
                )
                .expect("published view")
        };
        let view = publish("worker-target");
        if let Some(reported) = reported {
            let deployments = reported
                .iter()
                .map(|(id, lifecycle_generation)| PrivateModelDeploymentStatus {
                    model_deployment_id: (*id).to_owned(),
                    lifecycle_generation: *lifecycle_generation,
                    accelerator_memory: Some(reported_memory()),
                    ..Default::default()
                })
                .collect();
            coordinator
                .accept_status(
                    "worker-node",
                    &incarnation,
                    &NodeControllerStatus {
                        view: Some(view),
                        status_sequence: 1,
                        sections: vec![ControllerStatusSection {
                            payload: Some(controller_status_section::Payload::Model(
                                ModelControllerStatusSection {
                                    deployments,
                                    cleanup_evidence: Vec::new(),
                                },
                            )),
                        }],
                    },
                    |_| {
                        Ok::<_, ()>(NodeControllerStatusAcceptance {
                            accepted_status_sequence: 1,
                            receipts: Vec::new(),
                        })
                    },
                )
                .expect("accepted status");
        }
        publish("worker-target-added");
        coordinator
    }

    /// Memory every status of `node_coordinator` reports: usage on the node's device.
    fn reported_memory() -> AcceleratorMemoryObservation {
        use dotcore_proto::dotcore::deployment::v1::{
            AcceleratorMemoryDeviceUsage, AcceleratorMemoryUsageSet, accelerator_memory_observation,
        };

        AcceleratorMemoryObservation {
            observed_at: Some(dotcore_proto::google::protobuf::Timestamp {
                seconds: 10,
                nanos: 0,
            }),
            refresh: None,
            result: Some(accelerator_memory_observation::Result::Available(
                AcceleratorMemoryUsageSet {
                    devices: vec![AcceleratorMemoryDeviceUsage {
                        managed_node_id: "worker-node".to_owned(),
                        device_id: "GPU-a".to_owned(),
                        used_memory_bytes: 1024,
                    }],
                },
            )),
        }
    }

    fn listed_statuses(
        listed: &super::ListModelDeploymentsResponse,
    ) -> Vec<(ModelDeploymentStatus, &str)> {
        listed
            .deployments
            .iter()
            .map(|deployment| {
                let state = deployment.state.as_ref().expect("state");
                (
                    ModelDeploymentStatus::try_from(state.status).expect("status"),
                    state.status_message.as_str(),
                )
            })
            .collect()
    }
}
