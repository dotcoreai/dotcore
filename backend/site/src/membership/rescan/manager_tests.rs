//! Tests of the manager's side of a target re-scan: the invited node proposes through the private
//! handshake, the operator decides on that exact replacement, and the commit either records it once
//! or fails closed, leaving the site and the node's folder holding what they held before.

use std::{sync::Arc, time::Duration};

use dotcore_domain::{
    clock::{Clock, ManualClock},
    timing::RescanTiming,
};
use dotcore_proto::dotcore::{
    node::v1::{
        NodeObservationConfiguration, ProposeRescanRequest, ProposeRescanResponse, RescanNoOp,
        node_rescan_service_server::NodeRescanService as _, propose_rescan_response::Outcome,
    },
    site::v1::{
        ApproveRescanRequest, CancelRescanRequest, ExecutionTarget, RejectRescanRequest, Site,
        StartRescanRequest, site_rescan_service_server::SiteRescanService as _,
    },
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic::{Code, Request, Response, Status};

use super::commit::RescanSiteWriter;
use super::host::RescanService;
use super::management::RescanManagementService;
use super::registry::{RescanAttemptOutcome, RescanLimits, RescanRegistry, RescanStatus};
use super::test_fixtures::{
    RecordingSite, WORKER, WriteScript, committed, declare_model_deployment,
    docker_application_target, node_certificate, peer_address, retry_until, target_by_role,
    worker_site,
};
use super::{RescanBase, RescanRegistryError, rescan_base};
use crate::{
    membership::join::test_fixtures::InitializedSite,
    node::observation_configuration::NodeObservationConfigurationDigest,
    private_transport::{AuthenticatedMachineIdentity, PrivateConnectionInfo, PrivateNodeRole},
};

/// The invitation timing the handshake tests run on. Nothing here expires unless a test advances
/// the clock past it, and no watcher grace cuts a re-scan short.
fn test_timing() -> RescanTiming {
    RescanTiming {
        connect_timeout: Duration::from_secs(1),
        invitation_lifetime: Duration::from_secs(300),
        watcher_grace: Duration::from_secs(60),
        ended_retention: Duration::from_secs(60),
    }
}

/// A re-scan registry and its private service over an in-memory site declaring one worker with its
/// two targets, on a clock the test advances.
struct Harness {
    fixture: InitializedSite,
    site: Arc<RecordingSite>,
    clock: Arc<ManualClock>,
    registry: RescanRegistry,
    service: RescanService,
    /// Leaf certificate the worker's folder holds and its private connection proved.
    certificate_der: Vec<u8>,
    /// The whole-node base the site declares for the worker.
    base: RescanBase,
    /// The worker committed targets, in site order.
    committed: Vec<ExecutionTarget>,
}

impl Harness {
    fn new() -> Self {
        Self::build(test_timing(), None)
    }

    fn with_limits(limits: RescanLimits) -> Self {
        Self::build(test_timing(), Some(limits))
    }

    fn with_timing(timing: RescanTiming) -> Self {
        Self::build(timing, None)
    }

    fn build(timing: RescanTiming, limits: Option<RescanLimits>) -> Self {
        let (fixture, site) = worker_site();
        let (certificate_der, pin) = node_certificate();
        let writer = Arc::new(RecordingSite::new(site.clone(), pin));
        let clock = Arc::new(ManualClock::new());
        let mut registry = RescanRegistry::new(writer.clone(), timing, clock.clone());
        if let Some(limits) = limits {
            registry = registry.with_limits(limits);
        }
        let service = RescanService::new(registry.shared_for_test());
        let base = rescan_base(&site, WORKER).expect("the worker's whole-node base");
        Self {
            fixture,
            site: writer,
            clock,
            registry,
            service,
            certificate_der,
            base,
            committed: committed(&site),
        }
    }

    /// Starts the invitation every proposal answers, and returns what identifies it.
    async fn invite(&self) -> String {
        self.registry
            .start(WORKER)
            .await
            .expect("re-scan invitation")
            .rescan_id
    }

    /// What the site declares now.
    fn durable(&self) -> Site {
        self.site.site().expect("the durable site")
    }

    /// A whole replacement that differs from the committed targets: the model target is renamed and
    /// a Docker application target is added.
    fn replacement(&self) -> Vec<ExecutionTarget> {
        let mut targets = self.committed.clone();
        let model = target_by_role(&targets, "model");
        for target in &mut targets {
            if target.execution_target_id == model.execution_target_id {
                target.display_name = "Renamed model target".to_owned();
            }
        }
        targets.push(docker_application_target(WORKER));
        targets
    }

    /// The proposal the worker sends with `targets`, from a connection that proved its own
    /// certificate, plus the token reporting that connection closed.
    fn proposal(
        &self,
        targets: Vec<ExecutionTarget>,
        observation: Option<NodeObservationConfiguration>,
    ) -> (Request<ProposeRescanRequest>, CancellationToken) {
        self.proposal_as(
            WORKER,
            WORKER,
            &self.certificate_der.clone(),
            targets,
            observation,
        )
    }

    /// The proposal naming `node_id`, authenticated as `identity`, from a connection that proved
    /// its own certificate, and naming `folder_certificate` as its folder's own.
    fn proposal_as(
        &self,
        identity: &str,
        node_id: &str,
        folder_certificate: &[u8],
        targets: Vec<ExecutionTarget>,
        observation: Option<NodeObservationConfiguration>,
    ) -> (Request<ProposeRescanRequest>, CancellationToken) {
        let mut request = Request::new(ProposeRescanRequest {
            node_certificate_der: folder_certificate.to_vec(),
            node_id: node_id.to_owned(),
            targets,
            observation,
        });
        request
            .extensions_mut()
            .insert(AuthenticatedMachineIdentity {
                site_id: self.fixture.site.site_id.clone(),
                node_id: identity.to_owned(),
                role: PrivateNodeRole::Worker,
            });
        let (connection, closed) =
            PrivateConnectionInfo::presented(peer_address(), self.certificate_der.clone());
        request.extensions_mut().insert(connection);
        (request, closed)
    }

    /// Answers one proposal inline.
    async fn propose(
        &self,
        request: Request<ProposeRescanRequest>,
    ) -> Result<ProposeRescanResponse, Status> {
        self.service
            .propose_rescan(request)
            .await
            .map(Response::into_inner)
    }

    /// Runs one proposal in its own task, so the test can decide on it while it waits.
    fn propose_in_task(
        &self,
        request: Request<ProposeRescanRequest>,
    ) -> JoinHandle<Result<ProposeRescanResponse, Status>> {
        let rescans = self.registry.shared_for_test();
        tokio::spawn(async move {
            RescanService::new(rescans)
                .propose_rescan(request)
                .await
                .map(Response::into_inner)
        })
    }

    /// Waits for the re-scan to show a proposal under review and returns its identity, reporting the
    /// last state it observed when none appears.
    async fn under_review(&self, rescan_id: &str) -> String {
        let mut last = None;
        for _ in 0..1_000 {
            let mut watch = self.registry.watch(rescan_id).expect("the open re-scan");
            let status = watch.status();
            if let RescanStatus::AwaitingApproval { proposal_id, .. } = &status {
                return proposal_id.clone();
            }
            last = Some(status);
            tokio::task::yield_now().await;
        }
        panic!("no proposal was ever under review; last state {last:?}");
    }

    /// Waits for the re-scan to end committed and returns that state, reporting the last state it
    /// observed when it never does.
    async fn committed_status(&self, rescan_id: &str) -> RescanStatus {
        let mut watch = self.registry.watch(rescan_id).expect("the re-scan");
        let committed = async move {
            let mut status = watch.status();
            while !matches!(status, RescanStatus::Committed(_)) {
                status = watch.changed().await;
            }
            status
        };
        match tokio::time::timeout(Duration::from_secs(30), committed).await {
            Ok(status) => status,
            Err(_) => panic!(
                "the re-scan never ended committed; last state {:?}",
                self.status(rescan_id)
            ),
        }
    }

    /// The re-scan's current state.
    fn status(&self, rescan_id: &str) -> RescanStatus {
        self.registry
            .watch(rescan_id)
            .expect("the re-scan")
            .status()
    }

    /// The target ids of `targets`, for comparing what was recorded.
    fn ids(targets: &[ExecutionTarget]) -> Vec<String> {
        targets
            .iter()
            .map(|target| target.execution_target_id.clone())
            .collect()
    }
}

/// The worker's proposal is refused before anything is reviewed when its folder names a certificate
/// other than the identity the private connection proved.
#[tokio::test]
async fn a_proposal_naming_another_certificate_than_the_connection_proved_is_refused() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;

    let (request, _connection) = harness.proposal_as(
        WORKER,
        WORKER,
        b"another-node-certificate",
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await;

    assert_eq!(
        answer.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert_eq!(
        harness.status(&rescan_id),
        RescanStatus::Waiting { last_attempt: None }
    );
}

/// The proposal is refused when the authenticated connection belongs to a different node than the
/// one the proposal replaces.
#[tokio::test]
async fn a_proposal_authenticated_as_another_node_is_refused() {
    let harness = Harness::new();
    harness.invite().await;

    let (request, _connection) = harness.proposal_as(
        "node-other-worker",
        WORKER,
        &harness.certificate_der.clone(),
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await;

    assert_eq!(answer.expect_err("refusal").code(), Code::Unauthenticated);
    assert!(harness.site.recorded().is_empty());
}

/// A node the site does not declare has no whole-node base to replace, so its proposal is refused
/// as unknown even while another node's invitation is open.
#[tokio::test]
async fn a_proposal_from_a_node_the_site_does_not_declare_is_refused() {
    let harness = Harness::new();
    harness.invite().await;

    let (request, _connection) = harness.proposal_as(
        "node-gone",
        "node-gone",
        &harness.certificate_der.clone(),
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await;

    assert_eq!(answer.expect_err("refusal").code(), Code::NotFound);
    assert!(harness.site.recorded().is_empty());
}

/// A node whose folder installs no observation configuration cannot be compared against the site,
/// so its proposal is refused before any review.
#[tokio::test]
async fn a_proposal_without_an_observation_configuration_is_refused() {
    let harness = Harness::new();
    harness.invite().await;

    let (request, _connection) = harness.proposal(harness.replacement(), None);
    let answer = harness.propose(request).await;

    assert_eq!(
        answer.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
}

/// An invitation that expired refuses its node and records nothing, so a node that answered late
/// keeps the targets its folder already installs.
#[tokio::test]
async fn an_expired_invitation_refuses_its_node() {
    let timing = RescanTiming {
        invitation_lifetime: Duration::from_secs(5),
        ..test_timing()
    };
    let harness = Harness::with_timing(timing);
    let rescan_id = harness.invite().await;

    harness.clock.advance(Duration::from_secs(6));
    retry_until(|| matches!(harness.status(&rescan_id), RescanStatus::Expired)).await;

    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await;

    assert_eq!(
        answer.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
}

/// A node that was never invited has nothing to answer.
#[tokio::test]
async fn a_node_that_was_never_invited_is_refused() {
    let harness = Harness::new();

    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await;

    assert_eq!(
        answer.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
}

/// The site manager's own targets are not replaced over the private handshake: the invitation
/// refuses to start one.
#[tokio::test]
async fn a_site_manager_node_cannot_be_invited_to_re_scan() {
    let harness = Harness::new();
    let site = harness.durable();
    let manager = site
        .nodes
        .iter()
        .find(|node| node.site_manager.is_some())
        .expect("the site's manager node");

    let error = harness
        .registry
        .start(&manager.node_id)
        .await
        .expect_err("no invitation for the site manager");

    assert!(matches!(
        error,
        RescanRegistryError::ManagerNode { node_id } if node_id == manager.node_id
    ));
}

/// A refused proposal answers its node, records nothing, and leaves the invitation usable with the
/// refusal recorded as the latest answer.
#[tokio::test]
async fn an_unapproved_proposal_is_refused_and_the_site_keeps_its_targets() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let (request, _connection) =
        harness.proposal(replacement, Some(harness.base.observation.clone()));
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    let review = match harness.status(&rescan_id) {
        RescanStatus::AwaitingApproval { review, .. } => *review,
        status => panic!("expected a proposal under review, saw {status:?}"),
    };
    assert_eq!(review.node_id, WORKER);
    assert_eq!(review.classification.removed, Vec::new());
    assert_eq!(review.classification.added.len(), 1);
    assert_eq!(review.classification.changed.len(), 1);

    harness
        .registry
        .reject(&rescan_id, &proposal_id)
        .expect("refuse the proposal");
    let refusal = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    let status = harness.status(&rescan_id);
    assert!(matches!(
        status,
        RescanStatus::Waiting {
            last_attempt: Some(crate::membership::rescan::RescanAttempt {
                outcome: crate::membership::rescan::RescanAttemptOutcome::Refused,
                ..
            })
        }
    ));
}

/// An approved replacement is recorded once, and its node is answered the whole projection it must
/// install, including the digest of the observation configuration and the restart it needs.
#[tokio::test]
async fn an_approved_replacement_is_recorded_once_and_answered_to_its_node() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let (request, _connection) =
        harness.proposal(replacement.clone(), Some(harness.base.observation.clone()));
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let committed = answer.await.expect("the re-scan answer").expect("commit");

    let committed = match committed.outcome {
        Some(Outcome::Committed(committed)) => committed,
        outcome => panic!("expected the committed replacement, saw {outcome:?}"),
    };
    assert_eq!(committed.node_id, WORKER);
    assert_eq!(Harness::ids(&committed.targets), Harness::ids(&replacement));
    assert!(committed.restart_required);
    assert_eq!(
        committed.observation_configuration_sha256,
        NodeObservationConfigurationDigest::for_configuration(
            &committed
                .observation
                .expect("the committed observation configuration")
        )
        .expect("digest of the committed observation configuration")
        .as_str()
    );
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Committed(_)
    ));
}

/// A node that never received the answer to a proposal the site already recorded is handed that same
/// committed replacement once, and a third proposal under the same invitation is refused: the
/// retry converges without recording the targets twice.
#[tokio::test]
async fn a_lost_commit_answer_is_replayed_once_and_a_third_proposal_is_refused() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let observation = harness.base.observation.clone();

    let (request, _connection) = harness.proposal(replacement.clone(), Some(observation.clone()));
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let committed = answer.await.expect("the re-scan answer").expect("commit");

    let replay = harness
        .propose(
            harness
                .proposal(replacement.clone(), Some(observation.clone()))
                .0,
        )
        .await
        .expect("the replayed answer");
    assert_eq!(replay, committed);
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);

    let third = harness
        .propose(
            harness
                .proposal(replacement.clone(), Some(observation.clone()))
                .0,
        )
        .await
        .expect_err("no answer after the replay was used");
    assert_eq!(third.code(), Code::FailedPrecondition);
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
}

/// A node has one open re-scan at a time, and a new one starts as soon as the previous one ended,
/// while that ended one is still readable.
#[tokio::test]
async fn a_new_re_scan_starts_right_after_the_previous_one_ended() {
    let harness = Harness::new();
    let first = harness.invite().await;

    let refused = harness
        .registry
        .start(WORKER)
        .await
        .expect_err("the first re-scan is still open");
    assert!(matches!(refused, RescanRegistryError::AlreadyOpen));

    harness.registry.cancel(&first).expect("cancel the first");
    assert_eq!(harness.status(&first), RescanStatus::Cancelled);
    let second = harness.invite().await;
    assert_ne!(second, first);
    assert!(matches!(
        harness.status(&second),
        RescanStatus::Waiting { last_attempt: None }
    ));
}

/// Starting a new re-scan forgets the node's committed one, so a node that lost its answer is not
/// handed the old replacement past the new invitation: its proposal goes to the operator under the
/// new re-scan instead, and the site still records the targets once per approval.
#[tokio::test]
async fn a_new_re_scan_supersedes_the_committed_one_a_node_could_replay() {
    let harness = Harness::new();
    let first = harness.invite().await;
    let replacement = harness.replacement();
    let observation = harness.base.observation.clone();

    let (request, _connection) = harness.proposal(replacement.clone(), Some(observation.clone()));
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&first).await;
    harness
        .registry
        .approve(&first, &proposal_id)
        .expect("allow the proposal");
    answer.await.expect("the re-scan answer").expect("commit");

    let second = harness.invite().await;
    assert!(matches!(
        harness.registry.watch(&first),
        Err(RescanRegistryError::NotFound)
    ));

    let (request, _connection) = harness.proposal(replacement.clone(), Some(observation));
    let retry = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&second).await;
    assert!(!retry.is_finished());
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);

    harness
        .registry
        .reject(&second, &proposal_id)
        .expect("refuse the proposal");
    let refused = retry
        .await
        .expect("the re-scan answer")
        .expect_err("the refused proposal");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
}

/// A whole projection equal to what the site declares and the node installs answers unchanged,
/// writes no bytes anywhere, and asks for no restart.
#[tokio::test]
async fn an_unchanged_whole_projection_answers_unchanged_and_writes_no_bytes() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;

    let (request, _connection) = harness.proposal(
        harness.committed.clone(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose(request).await.expect("answer");

    assert_eq!(
        answer.outcome,
        Some(Outcome::NoOp(RescanNoOp {
            node_id: WORKER.to_owned(),
        }))
    );
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert_eq!(
        harness.status(&rescan_id),
        RescanStatus::NoOp {
            node_id: WORKER.to_owned()
        }
    );
}

/// A proposal whose base moved while the operator was asked is refused at the commit rather than
/// written over the newer declaration: the node must re-scan against the current base.
#[tokio::test]
async fn a_base_that_moved_before_the_commit_refuses_the_replacement() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    let mut moved = harness.durable();
    let mut extra = docker_application_target(WORKER);
    extra.execution_target_id = "node-worker-extra-target".to_owned();
    moved.execution_targets.push(extra);
    harness.site.replace_site(moved);

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let refusal = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(harness.site.target_ids(WORKER).len(), 3);
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A pin the site withdrew between the proposal and the commit refuses the commit: the node is no
/// longer the identity the site trusts, so its replacement is not recorded.
#[tokio::test]
async fn a_pin_the_site_withdrew_refuses_the_commit() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    harness.site.withdraw_pin();

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let refusal = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A replacement removing a target a deployment still uses is refused before the operator is asked,
/// and the site keeps every target the deployment needs.
#[tokio::test]
async fn a_replacement_removing_a_used_target_is_refused_before_the_operator_is_asked() {
    let harness = Harness::new();
    let model_target = target_by_role(&harness.committed, "model");
    let mut site = harness.durable();
    declare_model_deployment(&mut site, "worker-model", &model_target.execution_target_id);
    harness.site.replace_site(site);
    let rescan_id = harness.invite().await;

    let unused = harness
        .committed
        .iter()
        .filter(|target| target.execution_target_id != model_target.execution_target_id)
        .cloned()
        .collect::<Vec<_>>();
    let (request, _connection) = harness.proposal(unused, Some(harness.base.observation.clone()));
    let answer = harness.propose(request).await;

    assert_eq!(
        answer.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    let status = harness.status(&rescan_id);
    assert!(matches!(
        status,
        RescanStatus::Waiting {
            last_attempt: Some(crate::membership::rescan::RescanAttempt {
                outcome: crate::membership::rescan::RescanAttemptOutcome::Failed(_),
                ..
            })
        }
    ));
}

/// A second proposal while one awaits the operator is refused, so only one replacement of a node is
/// ever under review; the first one still commits.
#[tokio::test]
async fn a_second_proposal_while_one_awaits_the_operator_is_refused() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let observation = harness.base.observation.clone();
    let (request, _connection) = harness.proposal(harness.replacement(), Some(observation.clone()));
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    let (second, _second_connection) =
        harness.proposal(harness.committed.clone(), Some(observation));
    let refusal = harness.propose(second).await.expect_err("refusal");
    assert_eq!(refusal.code(), Code::FailedPrecondition);

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    answer
        .await
        .expect("the re-scan answer")
        .expect("the first proposal commits");
    assert_eq!(harness.site.recorded().len(), 1);
}

/// A node that closes its connection before the decision is answered as cancelled, and the
/// invitation stays usable: the retry under the same invitation commits exactly once.
#[tokio::test]
async fn a_node_that_closes_its_connection_can_retry_the_same_invitation() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let observation = harness.base.observation.clone();

    let (request, connection) = harness.proposal(replacement.clone(), Some(observation.clone()));
    let first = harness.propose_in_task(request);
    let _proposal_id = harness.under_review(&rescan_id).await;
    connection.cancel();
    let lost = first
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(lost.code(), Code::Cancelled);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.status(&rescan_id),
        RescanStatus::Waiting { last_attempt: None }
    );

    let (request, _connection) = harness.proposal(replacement.clone(), Some(observation));
    let retry = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the retried proposal");
    retry
        .await
        .expect("the re-scan answer")
        .expect("the retried proposal commits");

    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Committed(_)
    ));
}

/// Cancelling a re-scan ends the proposal its node is waiting on and refuses its invitation from
/// then on.
#[tokio::test]
async fn a_cancelled_re_scan_ends_the_proposal_its_node_is_waiting_on() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    harness.under_review(&rescan_id).await;
    harness
        .registry
        .cancel(&rescan_id)
        .expect("cancel the re-scan");
    let ended = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(ended.code(), Code::Aborted);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(harness.status(&rescan_id), RescanStatus::Cancelled);

    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    assert_eq!(
        harness.propose(request).await.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
}

/// A write that fails answers its node a failure, records nothing, and fails the re-scan, so no
/// later approval can claim a replacement the site never took.
#[tokio::test]
async fn a_failed_write_answers_a_failure_and_fails_the_re_scan() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    harness.site.set_write(WriteScript::Failed);
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let failure = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(failure.code(), Code::Internal);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A write whose outcome is unknown and did not reach the site fails closed: the node is answered
/// that the outcome is unknown and must not install anything it was not told was committed.
#[tokio::test]
async fn an_uncertain_write_that_did_not_reach_the_site_fails_closed() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    harness
        .site
        .set_write(WriteScript::Uncertain { landed: false });
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let failure = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(failure.code(), Code::Internal);
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A write whose outcome is unknown but did reach the site is answered by comparing the durable
/// declaration: the recorded replacement is committed and never rolled back.
#[tokio::test]
async fn an_uncertain_write_that_reached_the_site_is_answered_committed() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let (request, _connection) =
        harness.proposal(replacement.clone(), Some(harness.base.observation.clone()));
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    harness
        .site
        .set_write(WriteScript::Uncertain { landed: true });
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let committed = answer.await.expect("the re-scan answer").expect("commit");

    assert!(matches!(committed.outcome, Some(Outcome::Committed(_))));
    assert_eq!(harness.site.target_ids(WORKER), Harness::ids(&replacement));
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Committed(_)
    ));
}

/// Answers that failed validation are bounded: once a re-scan's allowance runs out it ends as
/// failed, so a node cannot keep offering replacements under one invitation.
#[tokio::test]
async fn a_re_scan_ends_failed_once_its_answers_ran_out() {
    let harness = Harness::with_limits(RescanLimits {
        max_active_rescans: 4,
        max_failed_attempts: 2,
    });
    let rescan_id = harness.invite().await;
    let model_target = target_by_role(&harness.committed, "model");
    let mut site = harness.durable();
    declare_model_deployment(&mut site, "worker-model", &model_target.execution_target_id);
    harness.site.replace_site(site);
    let unused = harness
        .committed
        .iter()
        .filter(|target| target.execution_target_id != model_target.execution_target_id)
        .cloned()
        .collect::<Vec<_>>();

    for _ in 0..2 {
        let (request, _connection) =
            harness.proposal(unused.clone(), Some(harness.base.observation.clone()));
        assert_eq!(
            harness.propose(request).await.expect_err("refusal").code(),
            Code::FailedPrecondition
        );
    }

    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
    let (request, _connection) = harness.proposal(unused, Some(harness.base.observation.clone()));
    assert_eq!(
        harness.propose(request).await.expect_err("refusal").code(),
        Code::FailedPrecondition
    );
    assert!(harness.site.recorded().is_empty());
}

/// The management surface starts an invitation for a declared worker, shows the proposal its node
/// sends, and carries the operator's decision to it.
#[tokio::test]
async fn the_management_surface_starts_an_invitation_and_decides_its_proposal() {
    let harness = Harness::new();
    let management = RescanManagementService::new(
        harness.registry.clone(),
        Arc::clone(&harness.clock) as Arc<dyn Clock>,
        Duration::from_secs(15),
    );

    let started = management
        .start_rescan(Request::new(StartRescanRequest {
            node_id: WORKER.to_owned(),
        }))
        .await
        .expect("invitation")
        .into_inner();
    assert!(!started.rescan_id.is_empty());
    assert!(started.expires_at.is_some());
    assert!(matches!(
        harness.status(&started.rescan_id),
        RescanStatus::Waiting { last_attempt: None }
    ));

    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&started.rescan_id).await;

    management
        .approve_rescan(Request::new(ApproveRescanRequest {
            rescan_id: started.rescan_id.clone(),
            proposal_id: proposal_id.clone(),
        }))
        .await
        .expect("approval");
    let committed = answer.await.expect("the re-scan answer").expect("commit");
    assert!(matches!(committed.outcome, Some(Outcome::Committed(_))));

    // A decision on a proposal that is no longer under review is refused, and so is a call about a
    // re-scan that ended.
    let stale = management
        .reject_rescan(Request::new(RejectRescanRequest {
            rescan_id: started.rescan_id.clone(),
            proposal_id,
        }))
        .await
        .expect_err("no proposal to refuse");
    assert_eq!(stale.code(), Code::NotFound);
    let unknown = management
        .cancel_rescan(Request::new(CancelRescanRequest {
            rescan_id: started.rescan_id,
        }))
        .await
        .expect_err("no open re-scan");
    assert_eq!(unknown.code(), Code::NotFound);
}

/// A decision naming a proposal other than the one under review is refused, so an operator cannot
/// decide a replacement the node no longer waits on.
#[tokio::test]
async fn a_decision_naming_another_proposal_is_refused() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);
    harness.under_review(&rescan_id).await;

    let error = harness
        .registry
        .approve(&rescan_id, "another-proposal")
        .expect_err("not the proposal under review");
    assert!(matches!(error, RescanRegistryError::StaleProposal));

    harness.registry.cancel(&rescan_id).expect("cancel");
    answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");
}

/// A node the site stopped declaring between its proposal and the approval has no whole-node base
/// left to replace, so the commit refuses its replacement instead of writing the node back.
#[tokio::test]
async fn a_node_the_site_stopped_declaring_is_refused_at_the_commit() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);

    let proposal_id = harness.under_review(&rescan_id).await;
    let mut removed = harness.durable();
    removed.nodes.retain(|node| node.node_id != WORKER);
    harness.site.replace_site(removed);

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let refusal = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(refusal.code(), Code::NotFound);
    assert!(harness.site.recorded().is_empty());
    assert_eq!(
        harness.site.target_ids(WORKER),
        Harness::ids(&harness.committed)
    );
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A deployment placed on a target the approved replacement changes, after the commit read the site
/// and before it wrote, refuses the whole replacement under the site lock and leaves the site as
/// the placement left it.
#[tokio::test]
async fn a_deployment_placed_while_the_commit_runs_refuses_the_replacement() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;
    let write = harness.site.hold_next_write();

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    write.reached().await;
    let mut placed = harness.durable();
    let model_target = target_by_role(&harness.committed, "model");
    declare_model_deployment(
        &mut placed,
        "worker-model",
        &model_target.execution_target_id,
    );
    harness.site.replace_site(placed.clone());
    write.release();
    let refusal = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert!(
        refusal.message().contains("worker-model"),
        "{}",
        refusal.message()
    );
    assert!(harness.site.recorded().is_empty());
    assert_eq!(harness.durable(), placed);
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Failed { .. }
    ));
}

/// A node whose connection drops while its approved replacement is being written still finds the
/// re-scan committed, and its retry is answered the recorded replacement once.
#[tokio::test]
async fn a_connection_dropped_during_the_commit_still_records_the_replacement() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let observation = harness.base.observation.clone();
    let (request, _connection) = harness.proposal(replacement.clone(), Some(observation.clone()));
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;
    let write = harness.site.hold_next_write();

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    write.reached().await;
    // The server drops the handler of a request whose connection went away.
    answer.abort();
    let dropped = answer.await.expect_err("the handler was dropped");
    assert!(dropped.is_cancelled());
    write.release();

    let status = harness.committed_status(&rescan_id).await;
    let RescanStatus::Committed(committed) = status else {
        unreachable!("the wait returns only a committed state");
    };
    assert_eq!(Harness::ids(&committed.targets), Harness::ids(&replacement));
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);

    let replay = harness
        .propose(harness.proposal(replacement.clone(), Some(observation)).0)
        .await
        .expect("the replayed answer");
    let replayed = match replay.outcome {
        Some(Outcome::Committed(replayed)) => replayed,
        outcome => panic!("expected the committed replacement, saw {outcome:?}"),
    };
    assert_eq!(Harness::ids(&replayed.targets), Harness::ids(&replacement));
    assert_eq!(
        replayed.observation_configuration_sha256,
        committed.observation_configuration_sha256
    );
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
}

/// A commit that cannot even read the node's pin ends the re-scan failed with the answer its node
/// got, rather than leaving it awaiting a decision on a proposal nobody waits on.
#[tokio::test]
async fn a_commit_that_cannot_read_the_pin_ends_the_re_scan_failed() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;

    harness.site.break_pin_reads();
    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    let failure = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");

    let reason = "the site could not record the replacement; nothing was written: the site trust could not be read";
    assert_eq!(failure.code(), Code::Internal);
    assert_eq!(failure.message(), reason);
    assert_eq!(
        harness.status(&rescan_id),
        RescanStatus::Failed {
            reason: reason.to_owned()
        }
    );
    assert!(harness.site.recorded().is_empty());
    harness.invite().await;
}

/// A cancel while the approved replacement is being written is refused as a failed precondition,
/// and the re-scan ends with what the site recorded.
#[tokio::test]
async fn a_cancel_while_the_approved_replacement_is_written_is_refused() {
    let harness = Harness::new();
    let management = RescanManagementService::new(
        harness.registry.clone(),
        Arc::clone(&harness.clock) as Arc<dyn Clock>,
        Duration::from_secs(15),
    );
    let rescan_id = harness.invite().await;
    let replacement = harness.replacement();
    let (request, _connection) =
        harness.proposal(replacement.clone(), Some(harness.base.observation.clone()));
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;
    let write = harness.site.hold_next_write();

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    write.reached().await;
    let refused = management
        .cancel_rescan(Request::new(CancelRescanRequest {
            rescan_id: rescan_id.clone(),
        }))
        .await
        .expect_err("a commit is in progress");
    assert_eq!(refused.code(), Code::FailedPrecondition);
    assert_eq!(
        refused.message(),
        "an approved replacement is being recorded; wait for the re-scan to end"
    );
    write.release();
    let committed = answer.await.expect("the re-scan answer").expect("commit");

    assert!(matches!(committed.outcome, Some(Outcome::Committed(_))));
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::Committed(_)
    ));
    assert_eq!(harness.site.recorded(), vec![Harness::ids(&replacement)]);
}

/// While an approved replacement is being written, neither a cancel nor a new re-scan of the node
/// gets in: a failed write ends only its own re-scan, and the next one starts waiting afresh.
#[tokio::test]
async fn a_failed_write_held_past_a_cancel_and_restart_ends_only_its_own_re_scan() {
    let harness = Harness::new();
    let first = harness.invite().await;
    let (request, _connection) = harness.proposal(
        harness.replacement(),
        Some(harness.base.observation.clone()),
    );
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&first).await;
    let write = harness.site.hold_next_write();
    harness.site.set_write(WriteScript::Failed);

    harness
        .registry
        .approve(&first, &proposal_id)
        .expect("allow the proposal");
    write.reached().await;
    assert!(matches!(
        harness.registry.cancel(&first),
        Err(RescanRegistryError::CommitInProgress)
    ));
    assert!(matches!(
        harness.registry.start(WORKER).await,
        Err(RescanRegistryError::AlreadyOpen)
    ));
    write.release();
    let failure = answer
        .await
        .expect("the re-scan answer")
        .expect_err("refusal");
    assert_eq!(failure.code(), Code::Internal);
    assert_eq!(
        harness.status(&first),
        RescanStatus::Failed {
            reason: "the site could not record the replacement; nothing was written: the site file could not be written".to_owned()
        }
    );

    let second = harness.invite().await;
    assert_eq!(
        harness.status(&second),
        RescanStatus::Waiting { last_attempt: None }
    );
    assert!(harness.site.recorded().is_empty());
}

/// A refusal the proposal sees only after the re-scan was cancelled leaves the re-scan cancelled.
#[tokio::test]
async fn a_refusal_seen_after_a_cancel_leaves_the_re_scan_cancelled() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let rescans = harness.registry.shared_for_test();
    let rescan = rescans.open_rescan_for(WORKER).expect("the open re-scan");
    let replacement = super::derive_replacement(
        &harness.durable(),
        &rescans.declaration_rules(),
        WORKER,
        harness.replacement(),
        Some(&harness.base),
    )
    .expect("a valid replacement");
    let claim = rescan
        .review(
            harness.base.clone(),
            replacement,
            harness.certificate_der.clone(),
        )
        .expect("the proposal under review");
    let proposal_id = harness.under_review(&rescan_id).await;

    harness
        .registry
        .reject(&rescan_id, &proposal_id)
        .expect("refuse the proposal");
    harness
        .registry
        .cancel(&rescan_id)
        .expect("cancel the re-scan");
    claim.finish(Some(RescanAttemptOutcome::Refused));

    assert_eq!(harness.status(&rescan_id), RescanStatus::Cancelled);
}

/// An answer that fails validation while another proposal awaits the operator leaves that review
/// standing, and the operator's decision on it still commits.
#[tokio::test]
async fn a_failed_answer_while_another_awaits_the_operator_keeps_the_review() {
    let harness = Harness::new();
    let rescan_id = harness.invite().await;
    let observation = harness.base.observation.clone();
    let (request, _connection) = harness.proposal(harness.replacement(), Some(observation.clone()));
    let answer = harness.propose_in_task(request);
    let proposal_id = harness.under_review(&rescan_id).await;

    let mut invalid = harness.replacement();
    invalid.push(docker_application_target(WORKER));
    let (second, _second_connection) = harness.proposal(invalid, Some(observation));
    let refusal = harness.propose(second).await.expect_err("refusal");
    assert_eq!(refusal.code(), Code::FailedPrecondition);
    assert!(matches!(
        harness.status(&rescan_id),
        RescanStatus::AwaitingApproval { proposal_id: shown, .. } if shown == proposal_id
    ));

    harness
        .registry
        .approve(&rescan_id, &proposal_id)
        .expect("allow the proposal");
    answer
        .await
        .expect("the re-scan answer")
        .expect("the first proposal commits");
    assert_eq!(harness.site.recorded().len(), 1);
}
