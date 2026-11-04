use std::time::Duration;

use dotcore_proto::dotcore::application::{
    private::v1::{
        ApplicationAssignment, ApplicationCleanupDisposition, ApplicationControllerStatusSection,
        ApplicationDeploymentStatus, ApplicationLifecycleObservation,
        ApplicationLinkedRouteProjection, ApplicationPublishedEndpoint,
        application_linked_route_projection,
    },
    v1::{ApplicationDeploymentDeclaration, ApplicationServiceAcceleratorMemory},
};
use dotcore_proto::{
    dotcore::{
        catalog::v1::OciPlatform,
        deployment::v1::{
            AcceleratorMemoryObservation, AcceleratorMemoryUnavailable, ExactPlacement,
            accelerator_memory_observation,
        },
        model::{
            private::v1::{
                ModelAssignment, ModelBindingReadiness, ModelCleanupDisposition,
                ModelCleanupEvidence, ModelControllerStatusSection, ModelDeploymentStatus,
                ModelDesiredViewSection, ModelLifecycleObservation, ModelPreparationProgress,
            },
            v1::{
                ModelDeploymentDeclaration, ModelDeploymentFailure,
                ModelDeploymentLifecycleTransition, ModelDeploymentSnapshot,
                ResolvedLocalModelDeploymentSnapshot, model_deployment_snapshot,
            },
        },
        site::{
            private::v1::{
                ControllerStatusReceipt, ControllerStatusSection, DesiredViewSection,
                DesiredViewSectionKind, NodeControlOpen, NodeControllerStatus,
                NodeControllerStatusAcceptance, NodeDesiredView, NodeViewAcknowledgement,
                NodeViewIdentity, NodeViewReference, RequiredDesiredViewSection,
                SupportedDesiredViewSection, controller_status_receipt, controller_status_section,
                desired_view_section, node_view_acknowledgement,
            },
            store::v1::NodeModelCleanupEvidence,
            v1::{
                ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter,
                SimulatedRuntimeDeploymentScenario, execution_target, model_runtime_binding,
            },
        },
    },
    google::protobuf::Empty,
};
use prost::Message;

use super::{
    DesiredViewContractError, MAX_ACCELERATOR_DEVICE_ID_BYTES, MAX_ACCELERATOR_MEMORY_DEVICES,
    MAX_APPLICATION_ASSIGNMENTS, MAX_FAILURE_CODE_BYTES, MAX_FAILURE_DIAGNOSTIC_BYTES,
    MAX_FAILURE_DIAGNOSTICS, MAX_FAILURE_MESSAGE_BYTES, MAX_STATUS_MESSAGE_BYTES,
    ValidatedNodeView, application_cleanup_evidence, application_cleanup_receipt,
    bounded_accelerator_memory, bounded_failure, bounded_status_message,
    cleanup_evidence_from_state, cleanup_receipt, validate_acknowledgement,
    validate_application_status, validate_controller_status, validate_published_view,
    validate_status_receipts,
};
use crate::view::fencing::{
    AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection, ViewAcceptance,
};

const MANAGER_SESSION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NODE_INCARNATION: &str = "11111111111111111111111111111111";

#[test]
fn required_section_manifest_fails_closed_and_explicit_empty_model_removes_intent() {
    let (open, mut view) = empty_view();
    let validated = validate(&open, &view).expect("empty model view");
    assert!(validated.projection.model_deployments().is_empty());
    assert!(validated.projection.execution_targets().is_empty());

    view.sections.clear();
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidSectionPayload)
    ));

    let (open, mut view) = empty_view();
    let required = view.required_sections[0];
    view.required_sections.push(required);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidRequiredSection)
    ));

    let (open, mut view) = empty_view();
    view.required_sections[0].schema_version = 2;
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::UnsupportedRequiredSection)
    ));

    let (open, mut view) = empty_view();
    view.required_sections[0].kind = 99;
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::UnsupportedRequiredSection)
    ));
}

#[test]
fn duplicate_typed_section_payload_is_rejected_before_installation() {
    let (open, mut view) = empty_view();
    let duplicate = view.sections[0].clone();
    view.sections.push(duplicate);

    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidSectionPayload)
    ));
}

#[test]
fn model_wire_projection_preserves_exact_local_launch_intent_without_resume_authorization() {
    let secret_launch_value = "assigned-secret-launch-value";
    let declaration = local_declaration(secret_launch_value);
    let (open, view) = assigned_view(declaration.clone());
    let encoded = view.encode_to_vec();
    assert!(
        encoded
            .windows(secret_launch_value.len())
            .any(|window| window == secret_launch_value.as_bytes())
    );

    let validated = validate(&open, &view).expect("assigned local view");
    assert_eq!(
        validated.projection.model_deployments(),
        std::slice::from_ref(&declaration)
    );
    // Each wire assignment is exactly its declaration field: nothing authorizes Resume.
    let assignments = match view.sections[0].payload.as_ref().expect("payload") {
        desired_view_section::Payload::Model(model) => &model.assignments,
        desired_view_section::Payload::Application(_) => panic!("model section"),
    };
    let mut declaration_only = Vec::new();
    prost::encoding::message::encode(1, &declaration, &mut declaration_only);
    assert_eq!(assignments[0].encode_to_vec(), declaration_only);

    let mut upstream = view;
    let assignment = match upstream.sections[0].payload.as_mut().expect("payload") {
        desired_view_section::Payload::Model(model) => &mut model.assignments[0],
        desired_view_section::Payload::Application(_) => panic!("model section"),
    };
    assignment
        .declaration
        .as_mut()
        .expect("declaration")
        .snapshot = Some(ModelDeploymentSnapshot {
        source: Some(model_deployment_snapshot::Source::Upstream(
            Default::default(),
        )),
    });
    assert!(matches!(
        validate(&open, &upstream),
        Err(DesiredViewContractError::InvalidModelAssignment)
    ));
}

#[test]
fn model_wire_projection_rejects_unassigned_target_workload_configuration() {
    let declaration = local_declaration("assigned-value");
    let (open, mut view) = assigned_view(declaration);
    let target = &mut view.execution_targets[0];
    target.runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
        ModelRuntimeBinding {
            adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                SimulatedRuntimeAdapter {
                    deployment_scenarios: vec![SimulatedRuntimeDeploymentScenario {
                        model_deployment_id: "unassigned-model".to_owned(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )),
        },
    ));

    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidModelAssignment)
    ));
}

#[test]
fn model_wire_projection_preserves_amd64_and_arm64_linux_targets() {
    for architecture in ["amd64", "arm64"] {
        let declaration = local_declaration("assigned-value");
        let (open, mut view) = assigned_view(declaration.clone());
        view.execution_targets[0]
            .platform
            .as_mut()
            .expect("platform")
            .architecture = architecture.to_owned();
        let projection = NodeViewProjection::new(view.execution_targets.clone(), vec![declaration])
            .expect("platform fixture projection");
        view.identity.as_mut().expect("identity").content_sha256 =
            projection.digest().as_str().to_owned();

        let validated = validate(&open, &view).expect("portable platform view");
        assert_eq!(
            validated.projection.execution_targets()[0]
                .platform
                .as_ref()
                .expect("validated platform")
                .architecture,
            architecture
        );
    }
}

#[test]
fn model_wire_projection_accepts_darwin_simulated_and_rejects_darwin_docker_targets() {
    for (docker, accepted) in [(false, true), (true, false)] {
        let declaration = local_declaration("assigned-value");
        let (open, mut view) = assigned_view(declaration.clone());
        let target = &mut view.execution_targets[0];
        let platform = target.platform.as_mut().expect("platform");
        platform.operating_system = "darwin".to_owned();
        platform.architecture = "arm64".to_owned();
        let adapter = if docker {
            model_runtime_binding::Adapter::DockerRuntime(Default::default())
        } else {
            model_runtime_binding::Adapter::SimulatedRuntime(Default::default())
        };
        target.runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(adapter),
            },
        ));

        let projection = NodeViewProjection::new(view.execution_targets.clone(), vec![declaration])
            .expect("darwin fixture projection");
        view.identity.as_mut().expect("identity").content_sha256 =
            projection.digest().as_str().to_owned();

        let result = validate(&open, &view);
        if accepted {
            result.expect("darwin simulated view");
        } else {
            assert!(matches!(
                result,
                Err(DesiredViewContractError::InvalidModelAssignment)
            ));
        }
    }
}

#[test]
fn an_unchanged_republication_is_revalidated_and_refreshes_the_installed_view() {
    let (open, first_view) = empty_view();
    let authority = authority();
    let stream = authority
        .open_stream(ManagerSession::parse(MANAGER_SESSION).expect("manager session"))
        .expect("stream");
    let first = validate_published_view(&open, &stream, &first_view).expect("first view");
    assert_eq!(
        authority.accept_publication(&stream, first.identity, &first.projection),
        Ok(ViewAcceptance::Installed)
    );

    let mut republished = first_view;
    republished
        .identity
        .as_mut()
        .expect("identity")
        .publication_sequence = 2;
    let republication =
        validate_published_view(&open, &stream, &republished).expect("unchanged full view");
    assert_eq!(republication.projection, first.projection);
    assert_eq!(republication.identity.revision, 1);
    assert_eq!(
        authority.accept_publication(&stream, republication.identity, &republication.projection),
        Ok(ViewAcceptance::Refreshed)
    );
}

#[test]
fn acknowledgements_need_a_nonzero_sequence_and_an_outcome() {
    let accepted = NodeViewAcknowledgement {
        view: Some(view_reference()),
        acknowledgement_sequence: 4,
        outcome: Some(node_view_acknowledgement::Outcome::Accepted(Empty {})),
    };
    validate_acknowledgement(&accepted).expect("accepted acknowledgement");

    let mut unsequenced = accepted.clone();
    unsequenced.acknowledgement_sequence = 0;
    assert!(matches!(
        validate_acknowledgement(&unsequenced),
        Err(DesiredViewContractError::InvalidSequence)
    ));

    let mut missing_outcome = accepted;
    missing_outcome.outcome = None;
    assert!(matches!(
        validate_acknowledgement(&missing_outcome),
        Err(DesiredViewContractError::MissingAcknowledgementOutcome)
    ));
}

#[test]
fn cleanup_dispositions_answer_exact_evidence_identity_and_survive_lost_delivery() {
    let status = status_report();
    let evidence = NodeModelCleanupEvidence {
        model_deployment_id: "model-gone".to_owned(),
        lifecycle_generation: 12,
        realization_generation: Some(5),
        never_realized: false,
        acknowledged: false,
    };
    let reported = cleanup_evidence_from_state(&evidence);
    assert_eq!(
        Some(&reported),
        status.sections[0]
            .payload
            .as_ref()
            .and_then(|payload| match payload {
                controller_status_section::Payload::Model(model) => {
                    model.cleanup_evidence.first()
                }
                controller_status_section::Payload::Application(_) => None,
            })
    );
    let manager = ManagerSession::parse(MANAGER_SESSION).expect("manager session");
    let node = NodeIncarnation::parse(NODE_INCARNATION).expect("node incarnation");
    let response = |disposition| NodeControllerStatusAcceptance {
        accepted_status_sequence: 9,
        receipts: vec![ControllerStatusReceipt {
            payload: Some(controller_status_receipt::Payload::Model(cleanup_receipt(
                &manager,
                &node,
                9,
                &reported,
                disposition,
            ))),
        }],
    };
    for disposition in [
        ModelCleanupDisposition::Acknowledged,
        ModelCleanupDisposition::Rejected,
    ] {
        validate_status_receipts(&status, &response(disposition)).expect("first delivery");
        validate_status_receipts(&status, &response(disposition)).expect("lost reply retry");
    }
    assert!(matches!(
        validate_status_receipts(&status, &response(ModelCleanupDisposition::Unspecified)),
        Err(DesiredViewContractError::ReceiptMismatch)
    ));

    let mut other_generation = response(ModelCleanupDisposition::Acknowledged);
    let Some(controller_status_receipt::Payload::Model(receipt)) =
        other_generation.receipts[0].payload.as_mut()
    else {
        panic!("model receipt")
    };
    receipt.lifecycle_generation = 13;
    assert!(matches!(
        validate_status_receipts(&status, &other_generation),
        Err(DesiredViewContractError::ReceiptMismatch)
    ));
}

#[test]
fn cleanup_evidence_identity_is_exactly_deployment_and_generation() {
    let evidence = |realization_generation| NodeModelCleanupEvidence {
        model_deployment_id: "model-gone".to_owned(),
        lifecycle_generation: 12,
        realization_generation,
        never_realized: realization_generation.is_none(),
        acknowledged: false,
    };
    let realized = cleanup_evidence_from_state(&evidence(Some(5)));
    let never_realized = cleanup_evidence_from_state(&evidence(None));

    assert_eq!(realized.evidence_sha256, never_realized.evidence_sha256);
    assert_ne!(
        realized.evidence_sha256,
        crate::store::cleanup_evidence_digest("model-gone", 13)
    );
    let mut status = status_report();
    let Some(controller_status_section::Payload::Model(model)) =
        status.sections[0].payload.as_mut()
    else {
        panic!("model status")
    };
    model.cleanup_evidence[0].evidence_sha256 = "c".repeat(64);
    assert!(matches!(
        validate_controller_status(&status),
        Err(DesiredViewContractError::InvalidStatusSection)
    ));
}

#[test]
fn binding_token_and_realization_generation_follow_binding_readiness() {
    let with_status = |edit: &dyn Fn(&mut ModelDeploymentStatus)| {
        let mut status = status_report();
        let Some(controller_status_section::Payload::Model(model)) =
            status.sections[0].payload.as_mut()
        else {
            panic!("model status")
        };
        edit(&mut model.deployments[0]);
        validate_controller_status(&status).map(drop)
    };
    let invalid = |edit: &dyn Fn(&mut ModelDeploymentStatus)| {
        assert!(matches!(
            with_status(edit),
            Err(DesiredViewContractError::InvalidStatusSection)
        ));
    };

    with_status(&|_| {}).expect("Ready status with a lowercase token");
    with_status(&|status| {
        status.binding_readiness = ModelBindingReadiness::NotReady as i32;
        status.binding_token.clear();
        status.realization_generation = None;
    })
    .expect("NotReady status without a token");
    invalid(&|status| status.binding_token = "B".repeat(64));
    invalid(&|status| status.binding_token = "b".repeat(63));
    invalid(&|status| status.binding_token = "g".repeat(64));
    invalid(&|status| status.binding_token.clear());
    invalid(&|status| status.realization_generation = None);
    invalid(&|status| status.binding_readiness = ModelBindingReadiness::NotReady as i32);
    invalid(&|status| {
        status.binding_readiness = ModelBindingReadiness::Absent as i32;
        status.binding_token = "x".to_owned();
    });
}

#[test]
fn byte_progress_validates_only_while_downloading() {
    let with_progress = |lifecycle: ModelLifecycleObservation, completed, total| {
        validate_preparation_status(|status| {
            status.lifecycle = lifecycle as i32;
            status.preparation_progress = Some(ModelPreparationProgress {
                completed_bytes: completed,
                total_bytes: total,
            });
        })
    };
    let downloading = ModelLifecycleObservation::Downloading;

    with_progress(downloading, 40, Some(100)).expect("partial download");
    with_progress(downloading, 100, Some(100)).expect("complete download");
    with_progress(downloading, 40, None).expect("download of unknown size");
    with_progress(downloading, 101, Some(100)).expect_err("completed beyond total");
    with_progress(downloading, 0, Some(0)).expect_err("zero total");
    for lifecycle in [
        ModelLifecycleObservation::Preparing,
        ModelLifecycleObservation::Loading,
        ModelLifecycleObservation::WarmingUp,
    ] {
        with_progress(lifecycle, 0, None).expect_err("progress outside Downloading");
    }
}

#[test]
fn failure_summary_is_reported_exactly_when_failed_and_within_its_sizes() {
    let failed = |failure: Option<ModelDeploymentFailure>| {
        validate_preparation_status(|status| {
            status.lifecycle = ModelLifecycleObservation::Failed as i32;
            status.failure = failure.clone();
        })
    };
    let failure = ModelDeploymentFailure {
        code: "engine_startup_failed".to_owned(),
        message: "Engine stopped during startup".to_owned(),
        diagnostics: vec!["Stage: engine startup".to_owned()],
    };

    failed(Some(failure.clone())).expect("failed status with its summary");
    failed(Some(ModelDeploymentFailure::default())).expect("failed status with an empty summary");
    failed(None).expect_err("failed status without a summary");
    validate_preparation_status(|status| status.failure = Some(failure.clone()))
        .expect_err("summary on a status that is not Failed");
    failed(Some(ModelDeploymentFailure {
        code: "c".repeat(MAX_FAILURE_CODE_BYTES + 1),
        ..failure.clone()
    }))
    .expect_err("oversized code");
    failed(Some(ModelDeploymentFailure {
        message: "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
        ..failure.clone()
    }))
    .expect_err("oversized message");
    failed(Some(ModelDeploymentFailure {
        diagnostics: vec![String::new(); MAX_FAILURE_DIAGNOSTICS + 1],
        ..failure.clone()
    }))
    .expect_err("too many diagnostics");
    failed(Some(ModelDeploymentFailure {
        diagnostics: vec![
            "d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES / 2),
            "d".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES / 2 + 1),
        ],
        ..failure
    }))
    .expect_err("oversized diagnostics");
    validate_preparation_status(|status| {
        status.status_message = "s".repeat(MAX_STATUS_MESSAGE_BYTES + 1);
    })
    .expect_err("oversized status message");
}

#[test]
fn hosting_node_cuts_recorded_text_to_the_sizes_the_manager_accepts() {
    let message = format!("{}é", "s".repeat(MAX_STATUS_MESSAGE_BYTES - 1));
    let failure = ModelDeploymentFailure {
        code: "c".repeat(MAX_FAILURE_CODE_BYTES + 5),
        message: "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 5),
        diagnostics: vec![
            "Stage: engine startup".to_owned(),
            format!("Output tail: {}", "t".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES)),
            "Exit status: 1".to_owned(),
        ],
    };

    let message = bounded_status_message(&message);
    let bounded = bounded_failure(&failure);

    assert_eq!(message, "s".repeat(MAX_STATUS_MESSAGE_BYTES - 1));
    assert_eq!(bounded.code, "c".repeat(MAX_FAILURE_CODE_BYTES));
    assert_eq!(bounded.message, "m".repeat(MAX_FAILURE_MESSAGE_BYTES));
    let tail_bytes = MAX_FAILURE_DIAGNOSTIC_BYTES - "Stage: engine startup".len();
    assert_eq!(
        bounded.diagnostics,
        [
            "Stage: engine startup".to_owned(),
            format!("Output tail: {}", "t".repeat(MAX_FAILURE_DIAGNOSTIC_BYTES))[..tail_bytes]
                .to_owned(),
        ]
    );
    validate_preparation_status(|status| {
        status.lifecycle = ModelLifecycleObservation::Failed as i32;
        status.status_message = message.clone();
        status.failure = Some(bounded.clone());
    })
    .expect("bounded text is accepted");
    assert_eq!(bounded_failure(&bounded), bounded);
}

#[test]
fn accelerator_memory_is_required_on_the_reporting_node_and_within_its_sizes() {
    let with_memory = |memory: Option<AcceleratorMemoryObservation>| {
        validate_preparation_status(|status| status.accelerator_memory = memory.clone())
    };
    let unavailable = |code: String, message: String| AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable { code, message },
        )),
        ..Default::default()
    };

    with_memory(Some(node_memory("worker-one"))).expect("usage on the reporting node");
    with_memory(None).expect_err("missing memory");
    with_memory(Some(AcceleratorMemoryObservation::default())).expect_err("missing result");
    with_memory(Some(node_memory("worker-two"))).expect_err("usage on another node");
    with_memory(Some(unavailable(
        "c".repeat(MAX_FAILURE_CODE_BYTES + 1),
        String::new(),
    )))
    .expect_err("oversized unavailable code");

    let bounded = bounded_accelerator_memory(
        "worker-one",
        unavailable(
            "c".repeat(MAX_FAILURE_CODE_BYTES + 1),
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
        ),
    );
    assert_eq!(
        bounded,
        unavailable(
            "c".repeat(MAX_FAILURE_CODE_BYTES),
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES)
        )
    );
    with_memory(Some(bounded)).expect("bounded unavailable reason");
}

#[test]
fn hosting_node_drops_accelerator_memory_rows_the_manager_would_reject() {
    let device = |node_id: &str, device_id: String| {
        dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryDeviceUsage {
            managed_node_id: node_id.to_owned(),
            device_id,
            used_memory_bytes: 1024,
        }
    };
    let usage = |devices| AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Available(
            dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryUsageSet { devices },
        )),
        ..Default::default()
    };
    let valid = (0..MAX_ACCELERATOR_MEMORY_DEVICES + 2)
        .map(|index| device("worker-one", format!("GPU-{index}")))
        .collect::<Vec<_>>();
    let mut reported = vec![
        device("worker-two", "GPU-other-node".to_owned()),
        device("worker-one", String::new()),
        device(
            "worker-one",
            "d".repeat(MAX_ACCELERATOR_DEVICE_ID_BYTES + 1),
        ),
    ];
    reported.extend(valid.iter().cloned());
    let unavailable_without_code = AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: String::new(),
                message: "Collection failed.".to_owned(),
            },
        )),
        ..Default::default()
    };

    let bounded = bounded_accelerator_memory("worker-one", usage(reported));
    let bounded_unavailable = bounded_accelerator_memory("worker-one", unavailable_without_code);

    assert_eq!(
        bounded,
        usage(valid[..MAX_ACCELERATOR_MEMORY_DEVICES].to_vec())
    );
    validate_preparation_status(|status| status.accelerator_memory = Some(bounded.clone()))
        .expect("bounded usage is accepted");
    assert_eq!(
        bounded_unavailable.result,
        Some(accelerator_memory_observation::Result::Unavailable(
            AcceleratorMemoryUnavailable {
                code: "UNSPECIFIED".to_owned(),
                message: "Collection failed.".to_owned(),
            }
        ))
    );
    validate_preparation_status(|status| {
        status.accelerator_memory = Some(bounded_unavailable.clone());
    })
    .expect("bounded unavailable reason is accepted");
}

#[test]
fn hosting_node_keeps_the_memory_refresh_outcome_and_bounds_a_failed_reason() {
    use dotcore_proto::dotcore::deployment::v1::accelerator_memory_refresh_state::Outcome;
    let with_outcome = |outcome: Outcome| AcceleratorMemoryObservation {
        refresh: Some(
            dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryRefreshState {
                attempted_at: Some(dotcore_proto::google::protobuf::Timestamp {
                    seconds: 1_800_000_000,
                    nanos: 0,
                }),
                outcome: Some(outcome),
            },
        ),
        ..node_memory("worker-one")
    };
    let failed = |code: String, message: String| {
        Outcome::Failed(AcceleratorMemoryUnavailable { code, message })
    };

    for outcome in [Outcome::Succeeded(Empty {}), Outcome::Pending(Empty {})] {
        let reported = with_outcome(outcome);
        assert_eq!(
            bounded_accelerator_memory("worker-one", reported.clone()),
            reported
        );
    }
    let bounded = bounded_accelerator_memory(
        "worker-one",
        with_outcome(failed(
            "c".repeat(MAX_FAILURE_CODE_BYTES + 1),
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES + 1),
        )),
    );

    assert_eq!(
        bounded,
        with_outcome(failed(
            "c".repeat(MAX_FAILURE_CODE_BYTES),
            "m".repeat(MAX_FAILURE_MESSAGE_BYTES),
        ))
    );
    validate_preparation_status(|status| status.accelerator_memory = Some(bounded.clone()))
        .expect("bounded refresh failure is accepted");
}

#[test]
fn rejection_and_status_wire_values_never_echo_assigned_launch_secrets() {
    let secret = "synthetic-required-launch-secret";
    let (_, assigned) = assigned_view(local_declaration(secret));
    assert!(
        assigned
            .encode_to_vec()
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    let acknowledgement = NodeViewAcknowledgement {
            view: Some(view_reference()),
            acknowledgement_sequence: 1,
            outcome: Some(node_view_acknowledgement::Outcome::Rejected(
                dotcore_proto::dotcore::site::private::v1::DesiredViewRejection {
                    code: dotcore_proto::dotcore::site::private::v1::DesiredViewRejectionCode::SectionPayloadInvalid as i32,
                },
            )),
        };
    let status = status_report();
    assert!(
        !acknowledgement
            .encode_to_vec()
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    assert!(
        !status
            .encode_to_vec()
            .windows(secret.len())
            .any(|window| window == secret.as_bytes())
    );
    assert!(!format!("{acknowledgement:?}{status:?}").contains(secret));
}

#[test]
fn application_section_is_explicit_on_application_nodes_and_empty_removes_only_its_intent() {
    let model = local_declaration("model-value");
    let (open, view) = mixed_view(vec![model.clone()], Vec::new());
    let validated = validate(&open, &view).expect("explicit empty application section");
    assert_eq!(
        validated.projection.model_deployments(),
        std::slice::from_ref(&model)
    );
    assert!(validated.projection.application_assignments().is_empty());

    let assignment = application_assignment("application-one", "application-target-one", 3);
    let (open, view) = mixed_view(vec![model.clone()], vec![assignment.clone()]);
    let validated = validate(&open, &view).expect("application intent");
    assert_eq!(
        validated.projection.model_deployments(),
        std::slice::from_ref(&model)
    );
    assert_eq!(validated.projection.application_assignments(), [assignment]);

    let (mut open, view) = mixed_view(vec![model.clone()], Vec::new());
    open.supported_sections.truncate(1);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::UnsupportedRequiredSection)
    ));

    let (open, mut view) = mixed_view(vec![model.clone()], Vec::new());
    view.required_sections.truncate(1);
    view.sections.truncate(1);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidRequiredSection)
    ));

    let (open, mut view) = mixed_view(vec![model], Vec::new());
    view.sections.truncate(1);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidSectionPayload)
    ));
}

#[test]
fn invalid_application_assignments_install_while_unattributable_ones_reject_the_view() {
    let model = local_declaration("model-value");
    let mut unknown_route = application_assignment("application-two", "application-target-one", 1);
    unknown_route
        .declaration
        .as_mut()
        .expect("declaration")
        .inference_route_id = Some("route-one".to_owned());
    unknown_route.linked_route = Some(ApplicationLinkedRouteProjection {
        resolution: Some(application_linked_route_projection::Resolution::Unknown(
            Empty {},
        )),
    });
    let invalid = vec![
        application_assignment("application-one", "application-target-one", 0),
        unknown_route,
        application_assignment("application-three", "target-one", 1),
        application_assignment("application-four", "missing-target", 1),
    ];
    let (open, view) = mixed_view(vec![model.clone()], invalid.clone());
    let validated = validate(&open, &view).expect("per-assignment problems never reject the view");
    assert_eq!(
        validated.projection.model_deployments(),
        std::slice::from_ref(&model)
    );
    assert_eq!(
        validated.projection.application_assignments().len(),
        invalid.len()
    );

    let (open, mut view) = mixed_view(
        vec![model.clone()],
        vec![application_assignment(
            "application-one",
            "application-target-one",
            1,
        )],
    );
    let Some(desired_view_section::Payload::Application(section)) =
        view.sections[1].payload.as_mut()
    else {
        panic!("application section")
    };
    let duplicate = section.assignments[0].clone();
    section.assignments.push(duplicate);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidSectionPayload)
    ));
    let Some(desired_view_section::Payload::Application(section)) =
        view.sections[1].payload.as_mut()
    else {
        panic!("application section")
    };
    section.assignments = vec![ApplicationAssignment::default()];
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidSectionPayload)
    ));

    let too_many = (0..=MAX_APPLICATION_ASSIGNMENTS)
        .map(|index| {
            application_assignment(&format!("application-{index}"), "application-target-one", 1)
        })
        .collect();
    let (open, view) = mixed_view(vec![model.clone()], too_many);
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::LimitExceeded)
    ));

    let mut on_application_target = model;
    on_application_target
        .placement
        .as_mut()
        .expect("model placement")
        .execution_target_id = "application-target-one".to_owned();
    let (open, view) = mixed_view(vec![on_application_target], Vec::new());
    assert!(matches!(
        validate(&open, &view),
        Err(DesiredViewContractError::InvalidModelAssignment)
    ));
}

#[test]
fn application_status_problems_never_reject_model_status_and_receipts_answer_its_evidence() {
    let evidence = application_cleanup_evidence("application-gone", 4, None);
    let mut status = status_report();
    status.sections.push(ControllerStatusSection {
        payload: Some(controller_status_section::Payload::Application(
            ApplicationControllerStatusSection {
                deployments: vec![application_status()],
                cleanup_evidence: vec![evidence.clone()],
            },
        )),
    });
    assert!(validate_controller_status(&status).is_ok());
    assert_eq!(
        validate_application_status(&status)
            .expect("valid application status")
            .map(|section| section.cleanup_evidence.clone()),
        Some(vec![evidence.clone()])
    );
    let receipt = |evidence| ControllerStatusReceipt {
        payload: Some(controller_status_receipt::Payload::Application(
            application_cleanup_receipt(
                &ManagerSession::parse(MANAGER_SESSION).expect("manager session"),
                &NodeIncarnation::parse(NODE_INCARNATION).expect("node incarnation"),
                9,
                evidence,
                ApplicationCleanupDisposition::Rejected,
            ),
        )),
    };
    assert!(
        validate_status_receipts(
            &status,
            &NodeControllerStatusAcceptance {
                accepted_status_sequence: 9,
                receipts: vec![receipt(&evidence)],
            },
        )
        .is_ok()
    );
    assert!(matches!(
        validate_status_receipts(
            &status,
            &NodeControllerStatusAcceptance {
                accepted_status_sequence: 9,
                receipts: vec![receipt(&application_cleanup_evidence(
                    "application-gone",
                    5,
                    None
                ))],
            },
        ),
        Err(DesiredViewContractError::ReceiptMismatch)
    ));

    let invalid_edits: [(&str, StatusEdit); 7] = [
        ("evidence digest", |section| {
            section.cleanup_evidence[0].evidence_sha256 = "c".repeat(64);
        }),
        ("never realized with a launch generation", |section| {
            section.cleanup_evidence[0].realization_generation = Some(1);
        }),
        ("placement on another node", |section| {
            section.deployments[0]
                .placement
                .as_mut()
                .expect("placement")
                .site_node_id = "worker-two".to_owned();
        }),
        ("endpoint host", |section| {
            section.deployments[0].endpoint = Some(ApplicationPublishedEndpoint {
                host: "node.example".to_owned(),
                port: 8080,
            });
        }),
        ("unit memory on another node", |section| {
            section.deployments[0].service_accelerator_memory[0].accelerator_memory =
                Some(node_memory("worker-two"));
        }),
        ("unit memory without a result", |section| {
            section.deployments[0].service_accelerator_memory[0].accelerator_memory = None;
        }),
        ("unit memory named twice", |section| {
            let unit = section.deployments[0].service_accelerator_memory[0].clone();
            section.deployments[0].service_accelerator_memory.push(unit);
        }),
    ];
    for (case, edit) in invalid_edits {
        let mut invalid = status.clone();
        let Some(controller_status_section::Payload::Application(section)) =
            invalid.sections[1].payload.as_mut()
        else {
            panic!("application status")
        };
        edit(section);
        assert!(validate_controller_status(&invalid).is_ok(), "{case}");
        assert!(
            matches!(
                validate_application_status(&invalid),
                Err(DesiredViewContractError::InvalidApplicationStatus)
            ),
            "{case}"
        );
    }

    let mut duplicated = status.clone();
    duplicated.sections.push(duplicated.sections[1].clone());
    assert!(matches!(
        validate_controller_status(&duplicated),
        Err(DesiredViewContractError::InvalidStatusSection)
    ));
}

type StatusEdit = fn(&mut ApplicationControllerStatusSection);

/// Validates `view` as a publication on a newly opened stream of the fixture manager session.
fn validate(
    open: &NodeControlOpen,
    view: &NodeDesiredView,
) -> Result<ValidatedNodeView, DesiredViewContractError> {
    let stream = authority()
        .open_stream(ManagerSession::parse(MANAGER_SESSION).expect("manager session"))
        .expect("stream");
    validate_published_view(open, &stream, view)
}

fn authority() -> AcceptedNodeViewAuthority {
    AcceptedNodeViewAuthority::new(
        "site-one",
        "worker-one",
        NodeIncarnation::parse(NODE_INCARNATION).expect("node incarnation"),
        ManagerSession::parse(MANAGER_SESSION).expect("manager session"),
        Duration::from_secs(60),
    )
    .expect("view authority")
}

fn empty_view() -> (NodeControlOpen, NodeDesiredView) {
    let projection = NodeViewProjection::new(Vec::new(), Vec::new()).expect("empty projection");
    let view = NodeDesiredView {
        identity: Some(NodeViewIdentity {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: MANAGER_SESSION.to_owned(),
            node_incarnation_id: NODE_INCARNATION.to_owned(),
            publication_sequence: 1,
            view_revision: 1,
            content_sha256: projection.digest().as_str().to_owned(),
            projection_schema_version: 1,
        }),
        required_sections: vec![RequiredDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
        }],
        sections: vec![DesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
            payload: Some(desired_view_section::Payload::Model(
                ModelDesiredViewSection::default(),
            )),
        }],
        execution_targets: Vec::new(),
    };
    (stream_open(), view)
}

fn assigned_view(declaration: ModelDeploymentDeclaration) -> (NodeControlOpen, NodeDesiredView) {
    let target = model_target();
    let projection = NodeViewProjection::new(vec![target.clone()], vec![declaration.clone()])
        .expect("assigned projection");
    let view = NodeDesiredView {
        identity: Some(NodeViewIdentity {
            site_id: "site-one".to_owned(),
            node_id: "worker-one".to_owned(),
            manager_session_id: MANAGER_SESSION.to_owned(),
            node_incarnation_id: NODE_INCARNATION.to_owned(),
            publication_sequence: 1,
            view_revision: 3,
            content_sha256: projection.digest().as_str().to_owned(),
            projection_schema_version: 1,
        }),
        required_sections: vec![RequiredDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
        }],
        sections: vec![DesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: 1,
            payload: Some(desired_view_section::Payload::Model(
                ModelDesiredViewSection {
                    assignments: vec![ModelAssignment {
                        declaration: Some(declaration),
                    }],
                },
            )),
        }],
        execution_targets: vec![target],
    };
    (stream_open(), view)
}

fn stream_open() -> NodeControlOpen {
    NodeControlOpen {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        node_incarnation_id: NODE_INCARNATION.to_owned(),
        supported_sections: vec![SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            minimum_schema_version: 1,
            maximum_schema_version: 1,
        }],
    }
}

fn local_declaration(secret_launch_value: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: "model-one".to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: "worker-one".to_owned(),
            execution_target_id: "target-one".to_owned(),
        }),
        snapshot: Some(ModelDeploymentSnapshot {
            source: Some(model_deployment_snapshot::Source::Local(
                ResolvedLocalModelDeploymentSnapshot {
                    custom_argument_tokens: vec![
                        "--assigned-value".to_owned(),
                        secret_launch_value.to_owned(),
                    ],
                    ..Default::default()
                },
            )),
        }),
        desired_state: dotcore_proto::dotcore::model::v1::ModelDeploymentDesiredState::Present
            as i32,
        lifecycle_generation: 7,
        lifecycle_transition: ModelDeploymentLifecycleTransition::Resume as i32,
        ..Default::default()
    }
}

fn model_target() -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: "target-one".to_owned(),
        site_node_id: "worker-one".to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding::default(),
        )),
        ..Default::default()
    }
}

/// A view for a node with a model and an application target, built by the manager publisher.
fn mixed_view(
    models: Vec<ModelDeploymentDeclaration>,
    applications: Vec<ApplicationAssignment>,
) -> (NodeControlOpen, NodeDesiredView) {
    let mut open = stream_open();
    open.supported_sections.push(SupportedDesiredViewSection {
        kind: DesiredViewSectionKind::Application as i32,
        minimum_schema_version: 1,
        maximum_schema_version: 1,
    });
    let mut application_target = model_target();
    application_target.execution_target_id = "application-target-one".to_owned();
    application_target.runtime_binding = Some(
        execution_target::RuntimeBinding::ApplicationRuntime(Default::default()),
    );
    let projection = NodeViewProjection::new(vec![model_target(), application_target], models)
        .expect("mixed projection")
        .with_application_assignments(applications)
        .expect("application projection");
    let view = crate::view::desired::node_desired_view(
        &ManagerSession::parse(MANAGER_SESSION).expect("manager session"),
        &NodeIncarnation::parse(NODE_INCARNATION).expect("node incarnation"),
        1,
        1,
        &projection,
        "site-one",
        "worker-one",
    );
    (open, view)
}

fn application_assignment(
    id: &str,
    target_id: &str,
    lifecycle_generation: u64,
) -> ApplicationAssignment {
    ApplicationAssignment {
        declaration: Some(ApplicationDeploymentDeclaration {
            application_deployment_id: id.to_owned(),
            application_deployment_name: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: target_id.to_owned(),
            }),
            lifecycle_generation,
            ..Default::default()
        }),
        linked_route: None,
    }
}

fn application_status() -> ApplicationDeploymentStatus {
    ApplicationDeploymentStatus {
        application_deployment_id: "application-one".to_owned(),
        lifecycle_generation: 3,
        placement: Some(ExactPlacement {
            site_node_id: "worker-one".to_owned(),
            execution_target_id: "application-target-one".to_owned(),
        }),
        lifecycle: ApplicationLifecycleObservation::Reconciling as i32,
        service_accelerator_memory: vec![ApplicationServiceAcceleratorMemory {
            service_name: "web".to_owned(),
            accelerator_memory: Some(node_memory("worker-one")),
        }],
        ..Default::default()
    }
}

fn view_reference() -> NodeViewReference {
    NodeViewReference {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        manager_session_id: MANAGER_SESSION.to_owned(),
        node_incarnation_id: NODE_INCARNATION.to_owned(),
        view_revision: 1,
        content_sha256: "b".repeat(64),
    }
}

/// Memory usage of 1 KiB on one device of `node_id`.
fn node_memory(node_id: &str) -> AcceleratorMemoryObservation {
    AcceleratorMemoryObservation {
        result: Some(accelerator_memory_observation::Result::Available(
            dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryUsageSet {
                devices: vec![
                    dotcore_proto::dotcore::deployment::v1::AcceleratorMemoryDeviceUsage {
                        managed_node_id: node_id.to_owned(),
                        device_id: "GPU-a".to_owned(),
                        used_memory_bytes: 1024,
                    },
                ],
            },
        )),
        ..Default::default()
    }
}

/// Validates the report after turning its deployment into a non-Ready status and applying
/// `edit`.
fn validate_preparation_status(
    edit: impl Fn(&mut ModelDeploymentStatus),
) -> Result<(), DesiredViewContractError> {
    let mut status = status_report();
    let Some(controller_status_section::Payload::Model(model)) =
        status.sections[0].payload.as_mut()
    else {
        panic!("model status")
    };
    let deployment = &mut model.deployments[0];
    deployment.binding_readiness = ModelBindingReadiness::NotReady as i32;
    deployment.binding_token.clear();
    deployment.realization_generation = None;
    edit(deployment);
    validate_controller_status(&status).map(drop)
}

fn status_report() -> NodeControllerStatus {
    NodeControllerStatus {
        view: Some(view_reference()),
        status_sequence: 9,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(
                ModelControllerStatusSection {
                    deployments: vec![ModelDeploymentStatus {
                        model_deployment_id: "model-one".to_owned(),
                        lifecycle_generation: 7,
                        placement: Some(ExactPlacement {
                            site_node_id: "worker-one".to_owned(),
                            execution_target_id: "target-one".to_owned(),
                        }),
                        realization_generation: Some(5),
                        lifecycle: ModelLifecycleObservation::Ready as i32,
                        status_message: "Ready".to_owned(),
                        preparation_progress: None,
                        binding_readiness: ModelBindingReadiness::Ready as i32,
                        failure: None,
                        state_revision: 4,
                        observed_placements: Vec::new(),
                        binding_token: "b".repeat(64),
                        resolved_launch_identities: Vec::new(),
                        accelerator_memory: Some(node_memory("worker-one")),
                    }],
                    cleanup_evidence: vec![ModelCleanupEvidence {
                        model_deployment_id: "model-gone".to_owned(),
                        lifecycle_generation: 12,
                        realization_generation: Some(5),
                        never_realized: false,
                        evidence_sha256: crate::store::cleanup_evidence_digest("model-gone", 12),
                    }],
                },
            )),
        }],
    }
}
