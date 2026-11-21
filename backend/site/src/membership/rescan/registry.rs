//! The running site manager's re-scan registry: the re-scans an operator started, each waiting
//! for one declared node to answer it, and the state the management API reports for each.
//!
//! Re-scans live in memory only, so a manager restart ends them all. A bounded number can be open
//! at once, and a node has at most one. Each invitation is short-lived: it expires, and one nobody
//! follows any more is cancelled after a short grace period. Only one proposal of a re-scan is
//! under review at a time, the first decision on a proposal wins, and a proposal that ended without
//! a commit leaves the invitation usable until its re-scan runs out of attempts. Once a proposal is
//! approved, only its commit ends the re-scan: a cancel is refused and an expiry waits for the
//! commit's outcome, so the re-scan always reports what the site recorded. An ended re-scan
//! stays readable for a while so a follower that reconnects sees how it ended, and so a node that
//! lost the answer to its proposal can be handed the same committed replacement once. Starting a
//! new re-scan of a node forgets its ended ones: from then on the new invitation alone answers that
//! node, and a node that lost an answer has the durable declaration decide under it instead.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::SystemTime,
};

use dotcore_domain::{clock::Clock, timing::RescanTiming};
use dotcore_proto::dotcore::{
    node::v1::{NodeObservationConfiguration, node_rescan_service_server::NodeRescanServiceServer},
    site::v1::{ExecutionTarget, Site},
};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::{
    RescanBase, RescanClassification, RescanReplacement,
    commit::{RescanCommitError, RescanSiteWriter},
    host::RescanService,
};

/// Largest proposal the rescan service decodes: one node's whole target set, far below this.
const MAX_RESCAN_MESSAGE_BYTES: usize = 256 * 1024;

/// Re-scans the site manager holds, shared by its private rescan service and its management API.
/// Clones share the same re-scans.
#[derive(Clone)]
pub struct RescanRegistry {
    shared: Arc<Rescans>,
}

impl RescanRegistry {
    /// Prepares a registry that reads and records through `site` and times its invitations on
    /// `timing`.
    pub fn new(
        site: Arc<dyn RescanSiteWriter>,
        timing: RescanTiming,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            shared: Arc::new(Rescans {
                site,
                clock,
                timing,
                limits: RescanLimits::default(),
                rescans: Mutex::default(),
            }),
        }
    }

    /// Starts a re-scan invitation for the declared worker `node_id`, which then waits for that
    /// node to answer it. Fails when the node is absent, is the site manager, already has a re-scan
    /// that has not ended, or too many re-scans are open. The node's ended re-scans are forgotten,
    /// so none of them replays a committed replacement past the new invitation.
    pub async fn start(&self, node_id: &str) -> Result<StartedRescan, RescanRegistryError> {
        let registry = &self.shared;
        let site = registry.site_snapshot().await?;
        let node = site
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .ok_or_else(|| RescanRegistryError::NotDeclared {
                node_id: node_id.to_owned(),
            })?;
        if node.site_manager.is_some() {
            return Err(RescanRegistryError::ManagerNode {
                node_id: node_id.to_owned(),
            });
        }

        let rescan = {
            let mut rescans = registry.lock_rescans();
            if rescans
                .values()
                .any(|rescan| rescan.answers(node_id) && !rescan.is_ended())
            {
                return Err(RescanRegistryError::AlreadyOpen);
            }
            if rescans.values().filter(|rescan| !rescan.is_ended()).count()
                >= registry.limits.max_active_rescans
            {
                return Err(RescanRegistryError::TooManyRescans);
            }
            let id = super::host::random_id().map_err(|_| RescanRegistryError::Identity)?;
            rescans.retain(|_, rescan| !rescan.answers(node_id));
            let rescan = Arc::new(ActiveRescan {
                id: id.clone(),
                node_id: node_id.to_owned(),
                expires_at: registry.clock.now() + registry.timing.invitation_lifetime,
                clock: Arc::clone(&registry.clock),
                max_failed_attempts: registry.limits.max_failed_attempts,
                status: watch::Sender::new(RescanStatus::Waiting { last_attempt: None }),
                progress: Mutex::new(RescanProgress::default()),
                watchers_changed: Notify::new(),
                ended: CancellationToken::new(),
            });
            rescans.insert(id, Arc::clone(&rescan));
            rescan
        };
        tokio::spawn(Arc::clone(registry).supervise(rescan.clone()));
        info!(rescan_id = %rescan.id, node_id = %node_id, "node re-scan started");
        Ok(StartedRescan {
            rescan_id: rescan.id.clone(),
            node_id: node_id.to_owned(),
            expires_at: registry.clock.wall_now() + registry.timing.invitation_lifetime,
        })
    }

    /// Follows one re-scan's state, including an ended re-scan still readable. The re-scan counts
    /// as followed while the returned watch lives.
    pub fn watch(&self, rescan_id: &str) -> Result<RescanWatch, RescanRegistryError> {
        let rescan = self.shared.rescan(rescan_id)?;
        let counted = {
            let mut progress = rescan.lock();
            if progress.ended {
                false
            } else {
                progress.watchers += 1;
                rescan.watchers_changed.notify_one();
                true
            }
        };
        Ok(RescanWatch {
            receiver: rescan.status.subscribe(),
            counted,
            rescan,
        })
    }

    /// Allows the proposal `proposal_id` of the re-scan, which then re-checks and records it.
    pub fn approve(&self, rescan_id: &str, proposal_id: &str) -> Result<(), RescanRegistryError> {
        self.shared
            .active_rescan(rescan_id)?
            .decide(proposal_id, RescanDecision::Allowed)
    }

    /// Refuses the proposal `proposal_id` of the re-scan, which then waits for a corrected answer
    /// under the same invitation.
    pub fn reject(&self, rescan_id: &str, proposal_id: &str) -> Result<(), RescanRegistryError> {
        self.shared
            .active_rescan(rescan_id)?
            .decide(proposal_id, RescanDecision::Refused)
    }

    /// Ends a re-scan that has not ended yet and refuses its invitation from then on. Refused
    /// while an approved replacement is being recorded: the re-scan then ends with that outcome.
    pub fn cancel(&self, rescan_id: &str) -> Result<(), RescanRegistryError> {
        self.shared
            .rescan(rescan_id)?
            .finish(RescanStatus::Cancelled)
    }

    /// Returns the rescan service the private listener hosts for nodes answering an invitation.
    /// It decodes only messages as small as one node's whole target set needs.
    pub fn rescan_service(&self) -> NodeRescanServiceServer<RescanService> {
        NodeRescanServiceServer::new(RescanService::new(Arc::clone(&self.shared)))
            .max_decoding_message_size(MAX_RESCAN_MESSAGE_BYTES)
    }

    /// Replaces the default limits on open re-scans and their attempts.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, limits: RescanLimits) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("the registry is not shared yet")
            .limits = limits;
        self
    }

    /// The shared state, for a test hosting the rescan service without a running site manager.
    #[cfg(test)]
    pub(super) fn shared_for_test(&self) -> Arc<Rescans> {
        Arc::clone(&self.shared)
    }
}

/// A re-scan invitation that was just started.
#[derive(Clone, Debug)]
pub struct StartedRescan {
    /// Identity of the re-scan, used by every other call.
    pub rescan_id: String,

    /// Node whose whole target set the invitation invites a replacement of.
    pub node_id: String,

    /// Wall time after which the invitation expires and the node can no longer answer it.
    pub expires_at: SystemTime,
}

/// State of one re-scan. Committed, no-op, cancelled, expired and failed are final.
#[derive(Clone, Debug, PartialEq)]
pub enum RescanStatus {
    /// No proposal is under review; the invitation waits for the node.
    Waiting {
        /// The latest answer that ended without a commit, if any.
        last_attempt: Option<RescanAttempt>,
    },

    /// A node passed the pin, base and validation checks and waits for the operator's decision.
    AwaitingApproval {
        /// Identity of this proposal, which a decision names.
        proposal_id: String,

        /// What the operator decides on.
        review: Box<RescanReview>,
    },

    /// The site recorded the replacement durably; the node installs it and restarts.
    Committed(Box<CommittedReplacement>),

    /// The whole projection equals what the node already installs: nothing was written.
    NoOp { node_id: String },

    /// The re-scan was cancelled, by the operator or because nobody followed it any more.
    Cancelled,

    /// The invitation expired before the node answered it.
    Expired,

    /// The re-scan ended without recording anything.
    Failed { reason: String },
}

impl RescanStatus {
    /// Reports whether the re-scan has ended and its state no longer changes.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Committed(_)
                | Self::NoOp { .. }
                | Self::Cancelled
                | Self::Expired
                | Self::Failed { .. }
        )
    }
}

/// One answer of a re-scan that ended without a commit, and when; the invitation stays usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RescanAttempt {
    /// How the answer ended.
    pub outcome: RescanAttemptOutcome,

    /// Wall time the answer ended, which tells two identical outcomes apart.
    pub at: SystemTime,
}

/// How one answer of a re-scan ended without a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RescanAttemptOutcome {
    /// The operator refused the proposal.
    Refused,

    /// The answer failed, for the reason given in words safe to show the operator.
    Failed(String),
}

/// What the operator decides on: one node's whole replacement, classified against its committed
/// targets. Deployments placed on a changed or removed target made the replacement refuse before
/// it was offered, so a review never lists one.
#[derive(Clone, Debug, PartialEq)]
pub struct RescanReview {
    /// Node whose targets the replacement replaces.
    pub node_id: String,

    /// Display name of that node.
    pub display_name: String,

    /// How the replacement differs from the committed targets.
    pub classification: RescanClassification,
}

/// The whole projection the site recorded for one committed replacement: the target set, the
/// refreshed observation configuration and its canonical digest.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedReplacement {
    /// Node the replacement was recorded for.
    pub node_id: String,

    /// Whole target set the node installs.
    pub targets: Vec<ExecutionTarget>,

    /// Refreshed observation configuration the node installs.
    pub observation: NodeObservationConfiguration,

    /// Canonical lowercase SHA-256 digest of the committed observation configuration.
    pub observation_configuration_sha256: String,
}

/// Follows one re-scan's state while it lives; the re-scan counts as followed meanwhile.
pub struct RescanWatch {
    receiver: watch::Receiver<RescanStatus>,
    counted: bool,
    rescan: Arc<ActiveRescan>,
}

impl RescanWatch {
    /// Returns the re-scan's current state.
    pub fn status(&mut self) -> RescanStatus {
        self.receiver.borrow_and_update().clone()
    }

    /// Waits for the next change of the re-scan's state and returns it. Never completes once the
    /// re-scan has ended.
    pub async fn changed(&mut self) -> RescanStatus {
        if self.receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        self.status()
    }
}

impl Drop for RescanWatch {
    fn drop(&mut self) {
        if self.counted {
            self.rescan.lock().watchers -= 1;
            self.rescan.watchers_changed.notify_one();
        }
    }
}

/// Bounds on open re-scans and on the answers of each one.
#[derive(Clone, Copy, Debug)]
pub struct RescanLimits {
    /// Re-scans open at once; starting another fails until one ends.
    pub max_active_rescans: usize,

    /// Answers of one re-scan that failed validation before it ends as failed. It bounds how often
    /// a node can make the manager validate a replacement before the operator sees one.
    pub max_failed_attempts: u32,
}

impl Default for RescanLimits {
    fn default() -> Self {
        Self {
            max_active_rescans: 8,
            max_failed_attempts: 5,
        }
    }
}

/// Why a management call about a re-scan was refused.
#[derive(Debug, thiserror::Error)]
pub enum RescanRegistryError {
    #[error("no such re-scan is open")]
    NotFound,

    #[error("the proposal is not the one awaiting a decision")]
    StaleProposal,

    #[error("this proposal was already decided")]
    AlreadyDecided,

    #[error("an approved replacement is being recorded; wait for the re-scan to end")]
    CommitInProgress,

    #[error("this node already has an open re-scan; wait for it to end or cancel it")]
    AlreadyOpen,

    #[error("too many re-scans are open; finish or cancel one first")]
    TooManyRescans,

    #[error("node {node_id:?} is not declared in this site")]
    NotDeclared { node_id: String },

    #[error(
        "node {node_id:?} is this site's manager; re-scan its targets with the command on this machine"
    )]
    ManagerNode { node_id: String },

    #[error("this re-scan could not be identified")]
    Identity,

    #[error("the site could not start a re-scan: {0}")]
    Site(#[source] RescanCommitError),
}

/// The registry state its rescan service and management calls share.
pub(super) struct Rescans {
    pub(super) site: Arc<dyn RescanSiteWriter>,
    clock: Arc<dyn Clock>,
    timing: RescanTiming,
    pub(super) limits: RescanLimits,
    rescans: Mutex<BTreeMap<String, Arc<ActiveRescan>>>,
}

impl Rescans {
    /// Returns the current site, off the request thread.
    pub(super) async fn site_snapshot(&self) -> Result<Site, RescanRegistryError> {
        let writer = Arc::clone(&self.site);
        super::host::blocking(move || writer.site())
            .await
            .map_err(|_| {
                RescanRegistryError::Site(RescanCommitError::Failed(anyhow::anyhow!(
                    "the site read stopped unexpectedly"
                )))
            })?
            .map_err(RescanRegistryError::Site)
    }

    /// Returns the owner rules the running site's declarations satisfy.
    pub(super) fn declaration_rules(&self) -> crate::validation::SiteDeclarationRules {
        self.site.declaration_rules()
    }

    /// Returns the re-scan answering `node_id`'s invitation that is not final yet, if any.
    pub(super) fn open_rescan_for(&self, node_id: &str) -> Option<Arc<ActiveRescan>> {
        self.lock_rescans()
            .values()
            .find(|rescan| rescan.answers(node_id) && !rescan.is_ended())
            .cloned()
    }

    /// Returns the committed replacement of a re-scan of `node_id` still readable that proposes this
    /// same whole target set, and has not been handed out again yet. The one replay is marked, so an
    /// identical proposal cannot keep collecting the same answer.
    pub(super) fn replay_committed(
        &self,
        node_id: &str,
        targets: &[ExecutionTarget],
    ) -> Option<CommittedReplacement> {
        let rescans = self.lock_rescans();
        rescans
            .values()
            .filter(|rescan| rescan.answers(node_id))
            .find_map(|rescan| {
                // The progress lock is taken before the state, the order every state change uses.
                let mut progress = rescan.lock();
                if progress.replayed {
                    return None;
                }
                let committed = match &*rescan.status.borrow() {
                    RescanStatus::Committed(committed) if committed.matches(targets) => {
                        (**committed).clone()
                    }
                    _ => return None,
                };
                progress.replayed = true;
                Some(committed)
            })
    }

    fn rescan(&self, rescan_id: &str) -> Result<Arc<ActiveRescan>, RescanRegistryError> {
        self.lock_rescans()
            .get(rescan_id)
            .cloned()
            .ok_or(RescanRegistryError::NotFound)
    }

    fn active_rescan(&self, rescan_id: &str) -> Result<Arc<ActiveRescan>, RescanRegistryError> {
        Some(self.rescan(rescan_id)?)
            .filter(|rescan| !rescan.is_ended())
            .ok_or(RescanRegistryError::NotFound)
    }

    /// Ends the re-scan when its invitation expires or nobody follows it for the grace period,
    /// then forgets it once it stayed readable long enough to replay its outcome. A re-scan whose
    /// approved replacement is being recorded ends with that commit's outcome instead.
    async fn supervise(self: Arc<Self>, rescan: Arc<ActiveRescan>) {
        let mut expiry = self.clock.sleep_until(rescan.expires_at);
        loop {
            let unwatched = rescan.lock().watchers == 0;
            let grace = async {
                if unwatched {
                    self.clock.sleep(self.timing.watcher_grace).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = rescan.ended.cancelled() => break,
                () = &mut expiry => {
                    let _ = rescan.finish(RescanStatus::Expired);
                    rescan.ended.cancelled().await;
                    break;
                }
                () = grace => {
                    if rescan.lock().watchers == 0 {
                        if rescan.finish(RescanStatus::Cancelled).is_ok() {
                            info!(rescan_id = %rescan.id, "node re-scan cancelled: nobody follows it");
                        }
                        rescan.ended.cancelled().await;
                        break;
                    }
                }
                () = rescan.watchers_changed.notified() => {}
            }
        }
        self.clock.sleep(self.timing.ended_retention).await;
        let mut rescans = self.lock_rescans();
        if rescans
            .get(&rescan.id)
            .is_some_and(|retained| Arc::ptr_eq(retained, &rescan))
        {
            rescans.remove(&rescan.id);
        }
    }

    fn lock_rescans(&self) -> MutexGuard<'_, BTreeMap<String, Arc<ActiveRescan>>> {
        self.rescans.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CommittedReplacement {
    /// Reports whether a proposal asks for exactly this committed replacement: the same whole target
    /// set, whose derived observation configuration can only match.
    fn matches(&self, targets: &[ExecutionTarget]) -> bool {
        self.targets == targets
    }
}

/// One re-scan the registry holds, open or ended.
pub(super) struct ActiveRescan {
    id: String,
    node_id: String,
    expires_at: tokio::time::Instant,
    clock: Arc<dyn Clock>,
    max_failed_attempts: u32,
    status: watch::Sender<RescanStatus>,
    progress: Mutex<RescanProgress>,
    watchers_changed: Notify,
    pub(super) ended: CancellationToken,
}

/// What one re-scan holds while its answers come and go.
#[derive(Default)]
struct RescanProgress {
    watchers: usize,

    /// The proposal under review, with the decision made on it; at most one.
    under_review: Option<Proposal>,

    /// Answers that failed validation, bounded by the attempt limit.
    failed_attempts: u32,

    /// Whether an ended re-scan already handed out its committed replacement once more.
    replayed: bool,

    ended: bool,
}

impl RescanProgress {
    /// Reports whether the proposal under review was approved, so its commit is under way.
    fn committing(&self) -> bool {
        self.under_review
            .as_ref()
            .is_some_and(|proposal| *proposal.decision.borrow() == Some(RescanDecision::Allowed))
    }

    /// Reports whether the proposal `proposal_id` is still the one under review.
    fn reviews(&self, proposal_id: &str) -> bool {
        self.under_review
            .as_ref()
            .is_some_and(|proposal| proposal.id == proposal_id)
    }
}

/// One proposal under review, and the decision the operator makes on it.
struct Proposal {
    id: String,
    decision: watch::Sender<Option<RescanDecision>>,
}

/// The operator's decision on one proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RescanDecision {
    Allowed,
    Refused,
}

impl ActiveRescan {
    fn answers(&self, node_id: &str) -> bool {
        self.node_id == node_id
    }

    fn is_ended(&self) -> bool {
        self.ended.is_cancelled()
    }

    fn lock(&self) -> MutexGuard<'_, RescanProgress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Moves the re-scan to `status` under its progress lock, ending it when that state is final.
    /// An ended re-scan keeps its final state.
    fn transition(&self, progress: &mut RescanProgress, status: RescanStatus) {
        if progress.ended {
            return;
        }
        let ends = status.is_final();
        // The status is recorded whether or not a console follows the re-scan, so a change is never
        // lost between watches.
        self.status.send_replace(status);
        if ends {
            progress.ended = true;
            progress.under_review = None;
            self.ended.cancel();
        }
    }

    /// Ends the re-scan in `status`, from outside any proposal. Refused once the re-scan ended, and
    /// while an approved replacement is being recorded, whose commit ends the re-scan instead.
    fn finish(&self, status: RescanStatus) -> Result<(), RescanRegistryError> {
        let mut progress = self.lock();
        if progress.ended {
            return Err(RescanRegistryError::NotFound);
        }
        if progress.committing() {
            return Err(RescanRegistryError::CommitInProgress);
        }
        self.transition(&mut progress, status);
        Ok(())
    }

    /// Puts `replacement`, checked against `base` for the node holding `certificate`, under review
    /// as the re-scan's only proposal.
    pub(super) fn review(
        self: &Arc<Self>,
        base: RescanBase,
        replacement: RescanReplacement,
        certificate: Vec<u8>,
    ) -> Result<ProposalClaim, tonic::Status> {
        let mut progress = self.lock();
        if progress.ended {
            return Err(tonic::Status::aborted("this re-scan has ended"));
        }
        if progress.under_review.is_some() {
            return Err(tonic::Status::failed_precondition(
                "another replacement of this node is under review; retry when it finishes",
            ));
        }
        let id = super::host::random_id()
            .map_err(|_| tonic::Status::internal("could not create a re-scan proposal"))?;
        let decision = watch::Sender::new(None);
        progress.under_review = Some(Proposal {
            id: id.clone(),
            decision: decision.clone(),
        });
        self.transition(
            &mut progress,
            RescanStatus::AwaitingApproval {
                proposal_id: id.clone(),
                review: Box::new(RescanReview {
                    node_id: self.node_id.clone(),
                    display_name: base.node.display_name.clone(),
                    classification: replacement.classification().clone(),
                }),
            },
        );
        drop(progress);
        Ok(ProposalClaim {
            rescan: Arc::clone(self),
            proposal_id: id,
            base,
            replacement,
            certificate,
            receiver: decision.subscribe(),
            finished: false,
        })
    }

    /// Records an answer that failed validation, ending the re-scan once its answers ran out. The
    /// state of a proposal still under review stands.
    pub(super) fn answer_failed(&self, reason: String) {
        let mut progress = self.lock();
        self.record_attempt(&mut progress, RescanAttemptOutcome::Failed(reason));
    }

    /// Ends the re-scan with the whole projection unchanged: nothing was written and the node needs
    /// no restart. Refused while another replacement of this node is under review, so a re-scan never
    /// ends under a proposal its node is still waiting on.
    pub(super) fn answer_no_op(&self) -> Result<(), tonic::Status> {
        let mut progress = self.lock();
        if progress.ended {
            return Err(tonic::Status::aborted("this re-scan has ended"));
        }
        if progress.under_review.is_some() {
            return Err(tonic::Status::failed_precondition(
                "another replacement of this node is under review; retry when it finishes",
            ));
        }
        self.transition(
            &mut progress,
            RescanStatus::NoOp {
                node_id: self.node_id.clone(),
            },
        );
        Ok(())
    }

    /// Records one answer that ended without a commit, ending the re-scan once its failed answers
    /// ran out. A re-scan whose approved replacement is being recorded ends with that commit instead.
    fn record_attempt(&self, progress: &mut RescanProgress, outcome: RescanAttemptOutcome) {
        let failed = matches!(outcome, RescanAttemptOutcome::Failed(_));
        if failed {
            progress.failed_attempts += 1;
        }
        if failed && progress.failed_attempts >= self.max_failed_attempts && !progress.committing()
        {
            self.transition(
                progress,
                RescanStatus::Failed {
                    reason: "too many replacement attempts failed".to_owned(),
                },
            );
            return;
        }
        if progress.under_review.is_some() {
            return;
        }
        self.transition(
            progress,
            RescanStatus::Waiting {
                last_attempt: Some(RescanAttempt {
                    outcome,
                    at: self.clock.wall_now(),
                }),
            },
        );
    }

    /// The operator's decision on the proposal `proposal_id`; the first one wins.
    fn decide(
        &self,
        proposal_id: &str,
        decision: RescanDecision,
    ) -> Result<(), RescanRegistryError> {
        let progress = self.lock();
        let proposal = progress
            .under_review
            .as_ref()
            .ok_or(RescanRegistryError::StaleProposal)?;
        if proposal.id != proposal_id {
            return Err(RescanRegistryError::StaleProposal);
        }
        if proposal.decision.borrow().is_some() {
            return Err(RescanRegistryError::AlreadyDecided);
        }
        let _ = proposal.decision.send(Some(decision));
        Ok(())
    }
}

/// The re-scan's proposal under review, held by the connection that proposed it. Each outcome it
/// records applies only while its proposal is still the one under review, so an ended or superseded
/// re-scan keeps its state. Dropping an unfinished claim frees the slot, so a node that vanished
/// mid-review doesn't hold it, and fails the re-scan when its approved commit stopped unrecorded.
pub(super) struct ProposalClaim {
    rescan: Arc<ActiveRescan>,
    proposal_id: String,
    pub(super) base: RescanBase,
    pub(super) replacement: RescanReplacement,
    certificate: Vec<u8>,
    receiver: watch::Receiver<Option<RescanDecision>>,
    finished: bool,
}

impl ProposalClaim {
    /// Waits for the operator's decision on this proposal. `None` when the proposal or the
    /// re-scan ended before any decision was made.
    pub(super) async fn await_decision(&mut self) -> Option<RescanDecision> {
        loop {
            if let Some(decision) = self.receiver.borrow_and_update().as_ref() {
                return Some(*decision);
            }
            if self.receiver.changed().await.is_err() {
                return None;
            }
        }
    }

    /// The certificate this node proved while proposing, whose pin the commit checks again.
    pub(super) fn certificate(&self) -> &[u8] {
        &self.certificate
    }

    /// Ends the proposal, recording `outcome` and freeing the slot; `None` leaves the re-scan
    /// waiting for the same proposal again without recording an attempt.
    pub(super) fn finish(mut self, outcome: Option<RescanAttemptOutcome>) {
        self.finished = true;
        let mut progress = self.rescan.lock();
        if !progress.reviews(&self.proposal_id) {
            return;
        }
        match outcome {
            Some(outcome) => {
                progress.under_review = None;
                self.rescan.record_attempt(&mut progress, outcome);
            }
            None => self.release(&mut progress),
        }
    }

    /// Records the committed replacement and ends the re-scan with it.
    pub(super) fn commit(mut self, committed: CommittedReplacement) {
        self.finished = true;
        let mut progress = self.rescan.lock();
        if progress.reviews(&self.proposal_id) {
            self.rescan
                .transition(&mut progress, RescanStatus::Committed(Box::new(committed)));
        }
    }

    /// Ends this proposal's re-scan as failed for `reason`, so its invitation stops accepting
    /// proposals with an approval that no longer holds. A re-scan that already ended keeps its state.
    pub(super) fn fail(&self, reason: &str) {
        let mut progress = self.rescan.lock();
        if progress.reviews(&self.proposal_id) {
            self.rescan.transition(
                &mut progress,
                RescanStatus::Failed {
                    reason: reason.to_owned(),
                },
            );
        }
    }

    fn release(&self, progress: &mut RescanProgress) {
        progress.under_review = None;
        self.rescan
            .transition(progress, RescanStatus::Waiting { last_attempt: None });
    }
}

impl Drop for ProposalClaim {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let mut progress = self.rescan.lock();
        if !progress.reviews(&self.proposal_id) {
            return;
        }
        if progress.committing() {
            self.rescan.transition(
                &mut progress,
                RescanStatus::Failed {
                    reason: "the replacement commit stopped before recording its outcome"
                        .to_owned(),
                },
            );
        } else {
            self.release(&mut progress);
        }
    }
}
