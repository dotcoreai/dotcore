use std::collections::BTreeMap;
use std::time::Duration;

use dotcore_proto::dotcore::application::private::v1::ApplicationCleanupDisposition;
use dotcore_proto::dotcore::{
    catalog::v1::OciPlatform,
    deployment::v1::ExactPlacement,
    model::v1::{
        ModelDeploymentDeclaration, ModelDeploymentDesiredState, ModelDeploymentLifecycleTransition,
    },
    site::{
        private::v1::{
            ControllerStatusSection, DesiredViewSectionKind, NodeViewReference,
            SupportedDesiredViewSection, controller_status_section,
        },
        v1::{
            ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter, Site, SiteNode,
            execution_target, model_runtime_binding,
        },
    },
};
use dotcore_proto::google::protobuf::Timestamp;

use super::*;
use crate::{private_transport::run_commit_section, view::contract::bounded_observed_placements};

#[test]
fn recovered_resume_publishes_its_declaration_without_resume_authorization() {
    let mut site = site();
    let mut resume = deployment("resume", "worker-one", "target-one");
    resume.desired_state = ModelDeploymentDesiredState::Present as i32;
    resume.snapshot = Some(dotcore_domain::test_support::model::local_model_snapshot(
        "resume-model",
    ));
    resume.lifecycle_transition = ModelDeploymentLifecycleTransition::Resume as i32;
    resume.lifecycle_generation = 4;
    site.model_deployments.push(resume.clone());

    let projection = project_complete_snapshot(&site)
        .expect("every node projects")
        .remove("worker-one")
        .expect("node projection");
    let view = node_desired_view(
        &ManagerSession::parse("a".repeat(32)).expect("manager session"),
        &NodeIncarnation::parse("1".repeat(32)).expect("node incarnation"),
        1,
        1,
        &projection,
        "site-one",
        "worker-one",
    );
    let Some(desired_view_section::Payload::Model(model)) = view
        .sections
        .first()
        .and_then(|section| section.payload.clone())
    else {
        panic!("model section")
    };

    assert_eq!(
        model.assignments,
        [ModelAssignment {
            declaration: Some(resume),
        }]
    );
}

#[test]
fn manager_restart_republishes_recovered_intent_under_a_fresh_session() {
    let projection = project_complete_snapshot(&site())
        .expect("projections")
        .remove("worker-one")
        .expect("node projection");
    let node = NodeIncarnation::parse("1".repeat(32)).expect("node incarnation");
    let old_session = ManagerSession::parse("a".repeat(32)).expect("old session");
    let new_session = ManagerSession::parse("b".repeat(32)).expect("new session");
    let old = node_desired_view(
        &old_session,
        &node,
        1,
        1,
        &projection,
        "site-one",
        "worker-one",
    );
    let new = node_desired_view(
        &new_session,
        &node,
        1,
        1,
        &projection,
        "site-one",
        "worker-one",
    );

    assert_eq!(view_revision(&old), 1);
    assert_eq!(view_revision(&new), 1);
    assert_ne!(
        old.identity.unwrap().manager_session_id,
        new.identity.unwrap().manager_session_id
    );
}

#[test]
fn node_reconstructs_published_resume_intent_exactly() {
    let mut site = site();
    let mut resume = deployment("resume", "worker-one", "target-one");
    resume.desired_state = ModelDeploymentDesiredState::Present as i32;
    resume.snapshot = Some(dotcore_domain::test_support::model::local_model_snapshot(
        "resume-model",
    ));
    resume.lifecycle_transition = ModelDeploymentLifecycleTransition::Resume as i32;
    resume.lifecycle_generation = 3;
    site.model_deployments.push(resume.clone());
    let published = project_requested_node(&site, "worker-one").expect("published node projection");
    let node = NodeIncarnation::parse("1".repeat(32)).expect("node incarnation");
    let manager = ManagerSession::parse("a".repeat(32)).expect("manager session");
    let authority = AcceptedNodeViewAuthority::new(
        "site-one",
        "worker-one",
        node.clone(),
        manager.clone(),
        Duration::from_secs(30),
    )
    .expect("reconstructed accepted-view authority");
    let stream = authority.open_stream(manager.clone()).expect("stream");
    let view = node_desired_view(&manager, &node, 1, 1, &published, "site-one", "worker-one");
    let installer = NodeViewInstaller::new(
        authority,
        std::slice::from_ref(&site.execution_targets[0]),
        owner_rules(),
    );

    installer
        .install_publication(&stream_open(), &stream, &view)
        .expect("reconstruct node from published Resume");

    assert_eq!(
        installer
            .projected_input()
            .declaration_snapshot()
            .expect("reconstructed node snapshot")
            .deployments,
        [resume]
    );
    assert_eq!(
        project_requested_node(&site, "worker-one")
            .expect("republished projection")
            .digest(),
        published.digest()
    );
}

#[tokio::test]
async fn rejected_target_mismatch_is_all_or_nothing_and_never_notifies() {
    let installed = model_target("target-one", "worker-one");
    let (authority, stream) = authority_and_stream();
    let installer = NodeViewInstaller::new(authority, &[installed], owner_rules());
    let projected = installer.projected_input();
    let listener = projected.register_change_listener();
    let different = model_target("target-two", "worker-one");
    let view = empty_view(1, 1, vec![different]);

    assert!(matches!(
        installer.install_publication(&stream_open(), &stream, &view),
        Err(DesiredViewDistributionError::InstalledTargetMismatch)
    ));
    assert!(projected.declaration_snapshot().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.notified())
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn an_unchanged_republication_refreshes_freshness_without_replacing_or_notifying_projection()
{
    let target = model_target("target-one", "worker-one");
    let node = NodeIncarnation::parse("1".repeat(32)).unwrap();
    let manager = ManagerSession::parse("a".repeat(32)).unwrap();
    let authority = AcceptedNodeViewAuthority::new(
        "site-one",
        "worker-one",
        node,
        manager.clone(),
        Duration::from_secs(10),
    )
    .unwrap();
    let stream = authority.open_stream(manager).unwrap();
    let installer = NodeViewInstaller::new(
        authority.clone(),
        std::slice::from_ref(&target),
        owner_rules(),
    );
    let listener = installer.projected_input().register_change_listener();

    let first = empty_view(1, 7, vec![target.clone()]);
    assert_eq!(
        installer
            .install_publication(&stream_open(), &stream, &first)
            .unwrap(),
        crate::view::fencing::ViewAcceptance::Installed
    );
    listener.notified().await;
    tokio::time::advance(Duration::from_secs(9)).await;
    let republished = empty_view(2, 7, vec![target]);
    assert_eq!(
        installer
            .install_publication(&stream_open(), &stream, &republished)
            .unwrap(),
        crate::view::fencing::ViewAcceptance::Refreshed
    );
    assert!(authority.action_permit().is_ok());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), listener.notified())
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_secs(11)).await;
    assert!(matches!(
        authority.action_permit(),
        Err(crate::view::fencing::ActionPermitError::Expired)
    ));
}

#[test]
fn reconnect_complete_view_removes_offline_intent_before_reconciliation_notification() {
    let target = model_target("target-one", "worker-one");
    let (authority, stream) = authority_and_stream();
    let installer = NodeViewInstaller::new(authority, std::slice::from_ref(&target), owner_rules());
    let installed_input = installer.projected_input();
    let view = empty_view(1, 2, vec![target]);

    installer
        .install_publication(&stream_open(), &stream, &view)
        .expect("complete reconnect view");

    assert!(
        installed_input
            .declaration_snapshot()
            .unwrap()
            .deployments
            .is_empty()
    );
}

#[test]
fn observed_placements_keep_only_the_reporting_node_rows_and_named_devices() {
    let placement =
        |node_id: &str, device_ids: &[&str]| dotcore_proto::dotcore::model::v1::ObservedPlacement {
            managed_node_id: node_id.to_owned(),
            accelerator_device_ids: device_ids.iter().map(|id| (*id).to_owned()).collect(),
        };

    assert_eq!(
        bounded_observed_placements(
            "worker-one",
            &[
                placement("worker-two", &["GPU-other-node"]),
                placement("worker-one", &["GPU-a", "", "GPU-b"]),
            ]
        ),
        [placement("worker-one", &["GPU-a", "GPU-b"])]
    );
}

#[tokio::test]
async fn stalled_status_commit_does_not_delay_another_node_publication_beyond_the_commit() {
    let (owner, mut gate) = TestStatusOwner::gated();
    let fixture = ManagerFixture::new(owner.clone());
    let worker_one_view = fixture
        .publish("worker-one")
        .await
        .expect("worker-one view");
    fixture
        .publish("worker-two")
        .await
        .expect("worker-two view");
    let status = {
        let fixture = fixture.clone();
        let request = status_for(&worker_one_view, 1);
        tokio::spawn(async move { fixture.report_status("worker-one", request).await })
    };
    gate.entered().await;

    let worker_two_publication = {
        let fixture = fixture.clone();
        tokio::spawn(async move { fixture.publish("worker-two").await })
    };
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
    assert!(!worker_two_publication.is_finished());
    gate.release();

    let worker_two = worker_two_publication
        .await
        .expect("worker-two publication task")
        .expect("worker-two publication after the commit");
    status
        .await
        .expect("worker-one status task")
        .expect("worker-one status");
    assert_eq!(view_revision(&worker_two), 1);
    assert!(
        !gate.watchdog_fired(),
        "the stalled commit blocked the executor"
    );
}

#[tokio::test]
async fn status_commit_completes_after_the_caller_disconnects() {
    let (owner, mut gate) = TestStatusOwner::gated();
    let fixture = ManagerFixture::new(owner.clone());
    let view = fixture
        .publish("worker-one")
        .await
        .expect("worker-one view");
    let request = status_for(&view, 1);
    let status = {
        let fixture = fixture.clone();
        let request = request.clone();
        tokio::spawn(async move { fixture.report_status("worker-one", request).await })
    };
    gate.entered().await;

    status.abort();
    assert!(
        status
            .await
            .expect_err("caller disconnected during the commit")
            .is_cancelled()
    );
    gate.release();
    owner.completed.notified().await;

    let replay = fixture
        .report_status("worker-one", request)
        .await
        .expect("exact retry returns the committed reply");
    assert_eq!(replay.accepted_status_sequence, 1);
    assert_eq!(owner.invocations(), 1);
    assert!(
        !gate.watchdog_fired(),
        "the stalled commit blocked the executor"
    );
}

#[tokio::test]
async fn linked_route_rename_republishes_the_application_section_and_nothing_else_does() {
    let routes = TestApplications::new();
    routes.name("route-one", "chat-one");
    let fixture = ManagerFixture::with_site(
        &application_site_yaml(),
        TestStatusOwner::scripted([]),
        &routes,
    );
    let first = fixture.publish("worker-one").await.expect("first view");
    assert_eq!(linked_route_names(&first), [Some("chat-one".to_owned())]);
    assert_eq!(
        view_revision(&fixture.publish("worker-one").await.expect("unchanged view")),
        view_revision(&first),
        "an unchanged route name keeps the published revision"
    );

    routes.name("route-one", "chat-two");
    let renamed = fixture.publish("worker-one").await.expect("renamed view");
    assert_eq!(view_revision(&renamed), view_revision(&first) + 1);
    assert_eq!(linked_route_names(&renamed), [Some("chat-two".to_owned())]);
    assert_eq!(model_section(&renamed), model_section(&first));

    routes.remove("route-one");
    let unknown = fixture
        .publish("worker-one")
        .await
        .expect("unknown-route view");
    assert_eq!(view_revision(&unknown), view_revision(&renamed) + 1);
    assert_eq!(linked_route_names(&unknown), [None]);

    let other = fixture
        .publish("worker-two")
        .await
        .expect("model-only node view");
    assert_eq!(
        other.required_sections,
        [RequiredDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            schema_version: MODEL_SECTION_SCHEMA_VERSION,
        }],
        "a node without application targets or intent keeps a model-only view"
    );
}

#[tokio::test]
async fn application_status_is_answered_per_entry_and_never_blocks_model_status() {
    let owner = TestStatusOwner::scripted([]);
    let fixture = ManagerFixture::new(owner.clone());
    let view = fixture
        .publish("worker-one")
        .await
        .expect("worker-one view");
    let evidence =
        crate::view::contract::application_cleanup_evidence("application-gone", 2, Some(1));
    let mut request = status_for(&view, 1);
    request.sections.push(ControllerStatusSection {
        payload: Some(controller_status_section::Payload::Application(
            ApplicationControllerStatusSection {
                deployments: Vec::new(),
                cleanup_evidence: vec![evidence.clone()],
            },
        )),
    });

    let response = fixture
        .report_status("worker-one", request.clone())
        .await
        .expect("status with application evidence");
    assert_eq!(owner.invocations(), 1);
    let reference = request.view.as_ref().expect("view");
    assert_eq!(
            response.receipts,
            [ControllerStatusReceipt {
                payload: Some(controller_status_receipt::Payload::Application(
                    crate::view::contract::application_cleanup_receipt(
                        &ManagerSession::parse(reference.manager_session_id.clone())
                            .expect("manager session"),
                        &NodeIncarnation::parse(reference.node_incarnation_id.clone())
                            .expect("node incarnation"),
                        1,
                        &evidence,
                        dotcore_proto::dotcore::application::private::v1::ApplicationCleanupDisposition::Acknowledged,
                    ),
                )),
            }]
        );

    let mut invalid = request;
    invalid.status_sequence = 2;
    let Some(controller_status_section::Payload::Application(application)) =
        invalid.sections[1].payload.as_mut()
    else {
        panic!("application status")
    };
    application.cleanup_evidence[0].evidence_sha256 = "c".repeat(64);
    let response = fixture
        .report_status("worker-one", invalid)
        .await
        .expect("an invalid application section never rejects model status");
    assert_eq!(owner.invocations(), 2);
    assert_eq!(response.accepted_status_sequence, 2);
    assert!(response.receipts.is_empty());
}

#[tokio::test]
async fn status_storage_failure_is_retryable_and_content_rejection_is_invalid() {
    let owner = TestStatusOwner::scripted([
        Err(ControllerStatusError::Retryable(anyhow::anyhow!(
            "injected state.pb write failure"
        ))),
        Err(ControllerStatusError::Retryable(anyhow::anyhow!(
            "injected uncertain rename"
        ))),
        Err(ControllerStatusError::Rejected(anyhow::anyhow!(
            "injected invalid argument"
        ))),
    ]);
    let fixture = ManagerFixture::new(owner.clone());
    let view = fixture
        .publish("worker-one")
        .await
        .expect("worker-one view");
    let request = status_for(&view, 1);
    let mut codes = Vec::new();

    for _ in 0..3 {
        codes.push(
            fixture
                .report_status("worker-one", request.clone())
                .await
                .expect_err("owner did not accept the report")
                .code(),
        );
    }
    let accepted = fixture
        .report_status("worker-one", request)
        .await
        .expect("uncommitted report is retried");

    assert_eq!(
        codes,
        [
            tonic::Code::Unavailable,
            tonic::Code::Unavailable,
            tonic::Code::InvalidArgument
        ]
    );
    assert_eq!(accepted.accepted_status_sequence, 1);
    assert_eq!(owner.invocations(), 4);
}

fn authority_and_stream() -> (AcceptedNodeViewAuthority, NodeViewStream) {
    let manager = ManagerSession::parse("a".repeat(32)).unwrap();
    let authority = AcceptedNodeViewAuthority::new(
        "site-one",
        "worker-one",
        NodeIncarnation::parse("1".repeat(32)).unwrap(),
        manager.clone(),
        Duration::from_secs(30),
    )
    .unwrap();
    let stream = authority.open_stream(manager).unwrap();
    (authority, stream)
}

fn stream_open() -> NodeControlOpen {
    NodeControlOpen {
        site_id: "site-one".to_owned(),
        node_id: "worker-one".to_owned(),
        node_incarnation_id: "1".repeat(32),
        supported_sections: vec![SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            minimum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
            maximum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
        }],
    }
}

/// A view of `targets` and no intent, published as `publication_sequence` at `revision`.
fn empty_view(
    publication_sequence: u64,
    revision: u64,
    targets: Vec<ExecutionTarget>,
) -> NodeDesiredView {
    let projection = NodeViewProjection::new(targets, Vec::new()).unwrap();
    node_desired_view(
        &ManagerSession::parse("a".repeat(32)).unwrap(),
        &NodeIncarnation::parse("1".repeat(32)).unwrap(),
        publication_sequence,
        revision,
        &projection,
        "site-one",
        "worker-one",
    )
}

fn site() -> Site {
    Site {
        site_id: "site-one".to_owned(),
        nodes: vec![
            SiteNode {
                node_id: "worker-one".to_owned(),
                ..Default::default()
            },
            SiteNode {
                node_id: "worker-two".to_owned(),
                ..Default::default()
            },
        ],
        execution_targets: vec![
            model_target("target-one", "worker-one"),
            model_target("target-two", "worker-two"),
        ],
        ..Default::default()
    }
}

fn project_complete_snapshot(
    site: &Site,
) -> Result<std::collections::BTreeMap<String, NodeViewProjection>, DesiredViewDistributionError> {
    site.nodes
        .iter()
        .map(|node| {
            Ok((
                node.node_id.clone(),
                project_requested_node(site, &node.node_id)?,
            ))
        })
        .collect()
}

fn model_target(id: &str, node_id: &str) -> ExecutionTarget {
    ExecutionTarget {
        execution_target_id: id.to_owned(),
        display_name: id.to_owned(),
        site_node_id: node_id.to_owned(),
        platform: Some(OciPlatform {
            operating_system: "linux".to_owned(),
            architecture: "amd64".to_owned(),
            variant: None,
        }),
        accelerator_device_ids: Vec::new(),
        runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
            ModelRuntimeBinding {
                adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                    SimulatedRuntimeAdapter::default(),
                )),
            },
        )),
    }
}

fn deployment(id: &str, node_id: &str, target_id: &str) -> ModelDeploymentDeclaration {
    ModelDeploymentDeclaration {
        model_deployment_id: id.to_owned(),
        model_deployment_name: id.to_owned(),
        placement: Some(ExactPlacement {
            site_node_id: node_id.to_owned(),
            execution_target_id: target_id.to_owned(),
        }),
        desired_state: ModelDeploymentDesiredState::Teardown as i32,
        lifecycle_generation: 1,
        lifecycle_transition: ModelDeploymentLifecycleTransition::Teardown as i32,
        ..Default::default()
    }
}

fn assigned(
    id: &str,
    desired_state: ModelDeploymentDesiredState,
    generation: u64,
) -> ModelDeploymentDeclaration {
    let mut declaration = deployment(id, "worker-one", "target-one");
    declaration.desired_state = desired_state as i32;
    declaration.lifecycle_generation = generation;
    declaration.lifecycle_transition = match desired_state {
        ModelDeploymentDesiredState::Paused => ModelDeploymentLifecycleTransition::Pause,
        ModelDeploymentDesiredState::Teardown => ModelDeploymentLifecycleTransition::Teardown,
        _ => ModelDeploymentLifecycleTransition::Create,
    } as i32;
    declaration.snapshot = Some(dotcore_domain::test_support::model::local_model_snapshot(
        id,
    ));
    declaration
}

/// Owner rules accepting every model assignment and classifying an application assignment
/// as a target mismatch unless it is placed on one of the node's application targets.
fn owner_rules() -> NodeViewOwnerRules {
    NodeViewOwnerRules {
        valid_model_assignment: |_| true,
        classify_application_assignment: |_, targets, assignment| {
            let placed = assignment
                .declaration
                .as_ref()
                .and_then(|declaration| declaration.placement.as_ref());
            targets
                .iter()
                .find(|target| {
                    placed.is_some_and(|placement| {
                        placement.execution_target_id == target.execution_target_id
                    })
                })
                .filter(|target| {
                    matches!(
                        target.runtime_binding,
                        Some(execution_target::RuntimeBinding::ApplicationRuntime(_))
                    )
                })
                .map(|_| ())
                .ok_or(ApplicationStatusFailureCode::TargetMismatch)
        },
    }
}

const TWO_WORKER_SITE: &str = r#"siteId: site-one
modelDeploymentIdentityAllocator:
  namespace: 0123456789abcdef
  nextSequence: '1'
nodes:
- nodeId: manager-node
  host: 127.0.0.1
  displayName: Manager node
  siteManager: {}
- nodeId: worker-one
  host: 127.0.0.2
  displayName: Worker one
- nodeId: worker-two
  host: 127.0.0.3
  displayName: Worker two
executionTargets:
- executionTargetId: target-one
  siteNodeId: worker-one
  displayName: target-one
  platform: { operatingSystem: linux, architecture: amd64 }
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
- executionTargetId: target-two
  siteNodeId: worker-two
  displayName: target-two
  platform: { operatingSystem: linux, architecture: amd64 }
  modelRuntime:
    simulatedRuntime:
      defaultScenario: { readyDelayMillis: 0, stopDelayMillis: 0 }
modelDeployments: []
applicationDeployments: []
"#;

/// Manager desired-view publisher over a real two-worker site and injected status owner, driven
/// through the commit sections a node control stream uses.
#[derive(Clone)]
struct ManagerFixture {
    _directory: Arc<tempfile::TempDir>,
    publisher: ManagerNodeViewPublisher,
}

impl ManagerFixture {
    fn new(owner: Arc<dyn ModelStatusOwner>) -> Self {
        Self::with_site(TWO_WORKER_SITE, owner, &TestApplications::new())
    }

    fn with_site(
        site_yaml: &str,
        owner: Arc<dyn ModelStatusOwner>,
        applications: &Arc<TestApplications>,
    ) -> Self {
        let directory =
            dotcore_domain::test_support::canonical_tempdir().expect("manager site directory");
        let site_path = directory.path().join("site.yaml");
        std::fs::write(&site_path, site_yaml).expect("two-worker site");
        let site = SiteRepository::load(&site_path, crate::test_support::site_declaration_rules())
            .expect("validated two-worker site");
        let application = Arc::clone(applications) as Arc<dyn ApplicationNodeViewOwner>;
        let coordinator = ManagerNodeControlCoordinator::new(
            ManagerSession::parse("a".repeat(32)).expect("manager session"),
        );
        for node_id in ["worker-one", "worker-two"] {
            register_node(&coordinator, node_id);
        }
        Self {
            _directory: Arc::new(directory),
            publisher: ManagerNodeViewPublisher::new(site, coordinator, owner, application),
        }
    }

    /// Opens a new stream of the node's registered incarnation and returns its first view.
    async fn publish(&self, node_id: &str) -> Result<NodeDesiredView, Status> {
        let publisher = self.publisher.clone();
        let opened = node_id.to_owned();
        let incarnation = NodeIncarnation::parse("1".repeat(32)).expect("node incarnation");
        run_commit_section(move || publisher.commit_stream_open(&opened, &incarnation))
            .await?
            .map(|(_, view)| view)
            .map_err(|error| rejected_exchange(node_id, "stream open", error))
    }

    /// Accepts one status of `node_id` as its node control stream does.
    async fn report_status(
        &self,
        node_id: &str,
        request: NodeControllerStatus,
    ) -> Result<NodeControllerStatusAcceptance, Status> {
        let publisher = self.publisher.clone();
        let reported = node_id.to_owned();
        run_commit_section(move || publisher.commit_status(&reported, &request))
            .await?
            .map_err(|error| rejected_exchange(node_id, "status", error))
    }
}

fn register_node(coordinator: &ManagerNodeControlCoordinator, node_id: &str) {
    let incarnation = NodeIncarnation::parse("1".repeat(32)).expect("node incarnation");
    coordinator
            .register_observation(
                &dotcore_proto::dotcore::site::private::v1::NodeObservation {
                    site_id: "site-one".to_owned(),
                    node_id: node_id.to_owned(),
                    incarnation_id: incarnation.as_str().to_owned(),
                    revision: 1,
                    snapshot: None,
                    observation_configuration_sha256: "0".repeat(64),
                    private_port: None,
                },
                crate::node::control::AcceptedNodeObservationInput {
                    node_id: node_id.to_owned(),
                    incarnation,
                    revision: 1,
                    snapshot: Default::default(),
                    observation_configuration_sha256:
                        crate::node::observation_configuration::NodeObservationConfigurationDigest::parse("0".repeat(64))
                            .expect("configuration digest"),
                    received_at: Timestamp::default(),
                    private_port: None,
                },
            )
            .expect("registered node");
}

fn status_for(view: &NodeDesiredView, status_sequence: u64) -> NodeControllerStatus {
    let identity = view.identity.as_ref().expect("view identity");
    NodeControllerStatus {
        view: Some(NodeViewReference {
            site_id: identity.site_id.clone(),
            node_id: identity.node_id.clone(),
            manager_session_id: identity.manager_session_id.clone(),
            node_incarnation_id: identity.node_incarnation_id.clone(),
            view_revision: identity.view_revision,
            content_sha256: identity.content_sha256.clone(),
        }),
        status_sequence,
        sections: vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(
                ModelControllerStatusSection::default(),
            )),
        }],
    }
}

fn view_revision(view: &NodeDesiredView) -> u64 {
    view.identity.as_ref().expect("view identity").view_revision
}

fn application_target(id: &str, node_id: &str) -> ExecutionTarget {
    let mut target = model_target(id, node_id);
    target.runtime_binding = Some(execution_target::RuntimeBinding::ApplicationRuntime(
            dotcore_proto::dotcore::site::v1::ApplicationRuntimeBinding {
                adapter: Some(
                    dotcore_proto::dotcore::site::v1::application_runtime_binding::Adapter::SimulatedRuntime(
                        Default::default(),
                    ),
                ),
            },
        ));
    target
}

fn application_assignment(id: &str, lifecycle_generation: u64) -> ApplicationAssignment {
    ApplicationAssignment {
        declaration: Some(ApplicationDeploymentDeclaration {
            application_deployment_id: id.to_owned(),
            application_deployment_name: id.to_owned(),
            placement: Some(ExactPlacement {
                site_node_id: "worker-one".to_owned(),
                execution_target_id: "application-target".to_owned(),
            }),
            lifecycle_generation,
            ..Default::default()
        }),
        linked_route: None,
    }
}

/// The two-worker site plus an application target on worker-one hosting one deployment
/// linked to `route-one`, whose member is a model on worker-one.
fn application_site_yaml() -> String {
    let mut site: Site = dotcore_proto::yaml::from_str(TWO_WORKER_SITE).expect("two-worker site");
    let platform = site.execution_targets[0].platform.clone();
    let mut target = application_target("application-target", "worker-one");
    target.platform = platform.clone();
    site.execution_targets.push(target);
    site.model_deployment_identity_allocator = Some(
        dotcore_proto::dotcore::site::v1::ModelDeploymentIdentityAllocator {
            namespace: "0123456789abcdef".to_owned(),
            next_sequence: 2,
        },
    );
    let mut model = assigned("model-one", ModelDeploymentDesiredState::Present, 1);
    model.requested_parallel_shape =
        dotcore_proto::dotcore::catalog::v1::ModelParallelShape::One as i32;
    site.model_deployments.push(model);
    site.inference_routes
        .push(dotcore_proto::dotcore::inference::v1::InferenceRoute {
            inference_route_id: "route-one".to_owned(),
            name: "route-one".to_owned(),
            enabled: true,
            fallback_groups: vec![dotcore_proto::dotcore::inference::v1::FallbackGroup {
                selection: dotcore_proto::dotcore::inference::v1::FallbackGroupSelection::Single
                    as i32,
                model_deployment_ids: vec!["model-one".to_owned()],
            }],
            generation: 1,
        });
    let mut application = application_assignment("application-one", 1)
        .declaration
        .expect("declaration");
    application.desired_state = ApplicationDeploymentDesiredState::Present as i32;
    application.inference_route_id = Some("route-one".to_owned());
    application.snapshot = Some(
        dotcore_proto::dotcore::application::v1::ResolvedApplicationDeploymentSnapshot {
            placement: application.placement.clone(),
            platform,
            ..Default::default()
        },
    );
    site.application_deployments.push(application);
    dotcore_proto::yaml::to_string(&site).expect("application site YAML")
}

/// Linked-route names of the application section, `None` for an unknown-route marker.
fn linked_route_names(view: &NodeDesiredView) -> Vec<Option<String>> {
    let Some(desired_view_section::Payload::Application(application)) = view
        .sections
        .get(1)
        .and_then(|section| section.payload.clone())
    else {
        panic!("application section")
    };
    application
        .assignments
        .iter()
        .map(|assignment| {
            match assignment
                .linked_route
                .as_ref()
                .and_then(|linked| linked.resolution.as_ref())
            {
                Some(application_linked_route_projection::Resolution::RouteName(name)) => {
                    Some(name.clone())
                }
                Some(application_linked_route_projection::Resolution::Unknown(_)) => None,
                None => panic!("linked route projection"),
            }
        })
        .collect()
}

fn model_section(view: &NodeDesiredView) -> Option<DesiredViewSection> {
    view.sections.first().cloned()
}

type StatusReply = Result<Vec<ControllerStatusReceipt>, ControllerStatusError>;

/// Status owner that replays scripted outcomes and can stall its first commit on a gate.
struct TestStatusOwner {
    replies: std::sync::Mutex<std::collections::VecDeque<StatusReply>>,
    gate: Option<StallGate>,
    invocations: std::sync::atomic::AtomicUsize,
    completed: tokio::sync::Notify,
}

impl TestStatusOwner {
    fn scripted(replies: impl IntoIterator<Item = StatusReply>) -> Arc<Self> {
        Arc::new(Self {
            replies: std::sync::Mutex::new(replies.into_iter().collect()),
            gate: None,
            invocations: std::sync::atomic::AtomicUsize::new(0),
            completed: tokio::sync::Notify::new(),
        })
    }

    fn gated() -> (Arc<Self>, StallGateControl) {
        let (gate, control) = StallGate::new();
        let owner = Arc::new(Self {
            replies: std::sync::Mutex::new(Default::default()),
            gate: Some(gate),
            invocations: std::sync::atomic::AtomicUsize::new(0),
            completed: tokio::sync::Notify::new(),
        });
        (owner, control)
    }

    fn invocations(&self) -> usize {
        self.invocations.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ModelStatusOwner for TestStatusOwner {
    fn accept_remote_status(
        &self,
        _: &str,
        _: &NodeViewProjection,
        _: &ModelControllerStatusSection,
        _: &ManagerSession,
        _: &NodeIncarnation,
        _: u64,
    ) -> Result<Vec<ControllerStatusReceipt>, ControllerStatusError> {
        if let Some(gate) = self.gate.as_ref() {
            gate.stall_once();
        }
        self.invocations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let reply = self
            .replies
            .lock()
            .expect("scripted replies")
            .pop_front()
            .unwrap_or_else(|| Ok(Vec::new()));
        self.completed.notify_one();
        reply
    }
}

/// Blocks the first owner commit until the test releases it. A watchdog thread releases it
/// anyway after a bounded wait so an executor blocked by the commit fails instead of hanging.
struct StallGate {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

struct StallGateControl {
    entered: Option<tokio::sync::oneshot::Receiver<()>>,
    release_requested: std::sync::mpsc::Sender<()>,
    watchdog_fired: Arc<std::sync::atomic::AtomicBool>,
}

impl StallGate {
    fn new() -> (Self, StallGateControl) {
        let (entered_sender, entered) = tokio::sync::oneshot::channel();
        let (release_sender, release) = std::sync::mpsc::channel();
        let (release_requested, requests) = std::sync::mpsc::channel::<()>();
        let watchdog_fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired = Arc::clone(&watchdog_fired);
        std::thread::spawn(move || {
            if requests.recv_timeout(Duration::from_secs(5)).is_err() {
                fired.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let _ = release_sender.send(());
        });
        (
            Self {
                entered: std::sync::Mutex::new(Some(entered_sender)),
                release: std::sync::Mutex::new(release),
            },
            StallGateControl {
                entered: Some(entered),
                release_requested,
                watchdog_fired,
            },
        )
    }

    fn stall_once(&self) {
        let Some(entered) = self.entered.lock().expect("stall gate").take() else {
            return;
        };
        let _ = entered.send(());
        let _ = self.release.lock().expect("stall gate").recv();
    }
}

impl StallGateControl {
    async fn entered(&mut self) {
        self.entered
            .take()
            .expect("single stall")
            .await
            .expect("owner commit entered");
    }

    fn release(&self) {
        let _ = self.release_requested.send(());
    }

    fn watchdog_fired(&self) -> bool {
        self.watchdog_fired
            .load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Application section owner linking each assignment to the route name the test sets, or
/// an unknown-route marker, and acknowledging every cleanup evidence entry it receives.
struct TestApplications {
    names: std::sync::Mutex<BTreeMap<String, String>>,
}

impl TestApplications {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            names: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn name(&self, inference_route_id: &str, route_name: &str) {
        self.names
            .lock()
            .expect("route names")
            .insert(inference_route_id.to_owned(), route_name.to_owned());
    }

    fn remove(&self, inference_route_id: &str) {
        self.names
            .lock()
            .expect("route names")
            .remove(inference_route_id);
    }
}

impl ApplicationNodeViewOwner for TestApplications {
    fn assignments(
        &self,
        declarations: &[ApplicationDeploymentDeclaration],
    ) -> Vec<ApplicationAssignment> {
        let names = self.names.lock().expect("route names");
        declarations
            .iter()
            .map(|declaration| ApplicationAssignment {
                declaration: Some(declaration.clone()),
                linked_route: declaration.inference_route_id.as_ref().map(|route_id| {
                    ApplicationLinkedRouteProjection {
                        resolution: Some(match names.get(route_id) {
                            Some(name) => {
                                application_linked_route_projection::Resolution::RouteName(
                                    name.clone(),
                                )
                            }
                            None => application_linked_route_projection::Resolution::Unknown(
                                dotcore_proto::google::protobuf::Empty {},
                            ),
                        }),
                    }
                }),
            })
            .collect()
    }

    fn accept_remote_status(
        &self,
        _: &str,
        _: &NodeViewProjection,
        application: &ApplicationControllerStatusSection,
        manager_session: &ManagerSession,
        node_incarnation: &NodeIncarnation,
        status_sequence: u64,
    ) -> Vec<ApplicationCleanupReceipt> {
        application
            .cleanup_evidence
            .iter()
            .map(|evidence| {
                crate::view::contract::application_cleanup_receipt(
                    manager_session,
                    node_incarnation,
                    status_sequence,
                    evidence,
                    ApplicationCleanupDisposition::Acknowledged,
                )
            })
            .collect()
    }
}
