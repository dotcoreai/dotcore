//! The site manager's join registry: the joins an operator started, each with its own token,
//! attempt limits and lifetime, and the state the management API reports for each.
//!
//! Joins live in memory only, so a manager restart ends them all. A bounded number can be active
//! at once; while any is, the private listener admits certificate-less machines to the join
//! service. Every proposal waits for the operator's decision, the first decision on it wins, and
//! a join nobody follows any more is cancelled after a short grace period. An ended join stays
//! readable for a while so a follower that reconnects sees how it ended.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError},
    time::SystemTime,
};

use dotcore_domain::{HostResolver, SystemHostResolver, clock::Clock, timing::JoinTiming};
use dotcore_proto::dotcore::{
    node::v1::node_join_service_server::NodeJoinServiceServer, site::v1::SiteNode,
};
use tokio::{
    sync::{Notify, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use url::Url;

use super::{
    JoinToken, JoinTokenError,
    host::{
        JoinDecision, JoinHostIdentity, JoinReview, JoinService, JoinSiteError, JoinSiteWriter,
        JoinedNode, PendingOffer, blocking, random_id,
    },
};
use crate::private_transport::{
    JoinGate, OpenJoinGate, PrivateConnectionInfo, authentication_failure,
};

/// Largest join or commit message the join service decodes. A join carries a signing request, a
/// certificate and a short proposal, far below this.
const MAX_JOIN_MESSAGE_BYTES: usize = 64 * 1024;

/// Joins the site manager hosts, shared by its join service and its management API. Clones
/// share the same joins.
#[derive(Clone)]
pub struct JoinRegistry {
    shared: Arc<Registry>,
}

impl JoinRegistry {
    /// Prepares a registry that presents `identity`, reads and records through `site`, and holds
    /// `gate` open while any join is active.
    pub fn new(
        identity: JoinHostIdentity,
        site: Arc<dyn JoinSiteWriter>,
        gate: JoinGate,
        timing: JoinTiming,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            shared: Arc::new(Registry {
                identity,
                site,
                resolver: Arc::new(SystemHostResolver),
                clock,
                timing,
                limits: JoinLimits::default(),
                gate,
                private_port: OnceLock::new(),
                joins: Mutex::default(),
            }),
        }
    }

    /// Records the port the private listener serves on, which every token names and every joined
    /// node keeps dialing. Joins start only once it is known.
    pub fn listen_on(&self, private_port: u16) {
        let _ = self.shared.private_port.set((private_port, private_port));
    }

    /// Records the port tokens name and, apart from it, the manager private port offers name, so a
    /// test hosts joins of a stopped site on a port of its own.
    #[cfg(any(test, feature = "test-support"))]
    pub fn listen_on_with_offered_port(&self, token_port: u16, offered_port: u16) {
        let _ = self.shared.private_port.set((token_port, offered_port));
    }

    /// Starts a join waiting for a machine, with a fresh token naming the declared manager host.
    pub async fn start(&self) -> Result<StartedJoin, JoinRegistryError> {
        let registry = &self.shared;
        let (port, _) = *registry
            .private_port
            .get()
            .ok_or(JoinRegistryError::NotListening)?;
        let site_writer = Arc::clone(&registry.site);
        let site = blocking(move || site_writer.site())
            .await
            .map_err(|_| {
                JoinRegistryError::Site(JoinSiteError::Failed(anyhow::anyhow!(
                    "the site read stopped unexpectedly"
                )))
            })?
            .map_err(JoinRegistryError::Site)?;
        let manager = site
            .nodes
            .iter()
            .find(|node| node.site_manager.is_some())
            .filter(|manager| {
                site.site_id == registry.identity.site_id()
                    && manager.node_id == registry.identity.manager_node_id()
            })
            .ok_or_else(|| {
                JoinRegistryError::Site(JoinSiteError::Failed(anyhow::anyhow!(
                    "the join identity is not this site's manager"
                )))
            })?;
        let endpoint =
            Url::parse(&format!("https://{}:{port}", manager.host)).map_err(|source| {
                JoinRegistryError::Site(JoinSiteError::Failed(
                    anyhow::Error::new(source).context("compose the join endpoint"),
                ))
            })?;
        let token = registry
            .identity
            .token(endpoint)
            .map_err(JoinRegistryError::Token)?;
        let join_id = random_id()
            .map_err(|source| JoinRegistryError::Token(JoinTokenError::Randomness(source)))?;
        let lifetime = registry.timing.token_lifetime;
        let expires_at = registry.clock.wall_now() + lifetime;

        let join = {
            let mut joins = registry.lock_joins();
            let active = joins.values().filter(|join| !join.is_ended()).count();
            if active >= registry.limits.max_active_joins {
                return Err(JoinRegistryError::TooManyJoins);
            }
            let join = Arc::new(ActiveJoin {
                id: join_id.clone(),
                token: token.clone(),
                expires_at: registry.clock.now() + lifetime,
                status: watch::Sender::new(JoinStatus::Waiting { last_attempt: None }),
                progress: Mutex::new(JoinProgress {
                    gate: Some(registry.gate.open()),
                    ..JoinProgress::default()
                }),
                watchers_changed: Notify::new(),
                ended: CancellationToken::new(),
            });
            joins.insert(join_id.clone(), Arc::clone(&join));
            join
        };
        tokio::spawn(Arc::clone(registry).supervise(join));
        info!(join_id = %join_id, "node join started");
        Ok(StartedJoin {
            join_id,
            token,
            expires_at,
        })
    }

    /// Follows one join's state, including an ended join still readable. The join counts as
    /// followed while the returned watch lives.
    pub fn watch(&self, join_id: &str) -> Result<JoinWatch, JoinRegistryError> {
        let join = self.shared.join(join_id)?;
        let watcher = {
            let mut progress = join.lock();
            (!progress.ended).then(|| {
                progress.watchers += 1;
                join.watchers_changed.notify_one();
            })
        };
        Ok(JoinWatch {
            receiver: join.status.subscribe(),
            counted: watcher.is_some(),
            join,
        })
    }

    /// Allows the proposal `proposal_id` of the join, which then signs and offers the node.
    pub fn approve(&self, join_id: &str, proposal_id: &str) -> Result<(), JoinRegistryError> {
        self.shared
            .active_join(join_id)?
            .decide(proposal_id, JoinDecision::Allow)
    }

    /// Refuses the proposal `proposal_id` of the join, which then waits for a corrected attempt.
    pub fn reject(&self, join_id: &str, proposal_id: &str) -> Result<(), JoinRegistryError> {
        self.shared
            .active_join(join_id)?
            .decide(proposal_id, JoinDecision::Reject)
    }

    /// Ends a join that has not ended yet and refuses its token from then on. A commit in
    /// progress reports its own outcome first.
    pub fn cancel(&self, join_id: &str) -> Result<(), JoinRegistryError> {
        self.shared.active_join(join_id)?.finish(JoinEnd::Cancelled);
        Ok(())
    }

    /// Returns the join service the private listener serves to joining machines. It decodes
    /// only messages as small as a join needs, since any machine reaching the port may send one.
    pub fn join_service(&self) -> NodeJoinServiceServer<JoinService> {
        NodeJoinServiceServer::new(JoinService::new(Arc::clone(&self.shared)))
            .max_decoding_message_size(MAX_JOIN_MESSAGE_BYTES)
    }

    /// Replaces the system resolver used to look up a node host name before dialing it back and
    /// comparing it with the join connection's address.
    #[cfg(test)]
    pub(crate) fn with_host_resolver(mut self, resolver: Arc<dyn HostResolver>) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("the registry is not shared yet")
            .resolver = resolver;
        self
    }

    /// Returns the state the join service shares, for checks a test makes without a listener.
    #[cfg(test)]
    pub(super) fn shared_for_test(&self) -> &Registry {
        &self.shared
    }

    /// Replaces the default limits on joins and their attempts.
    #[cfg(test)]
    pub(crate) fn with_limits(mut self, limits: JoinLimits) -> Self {
        Arc::get_mut(&mut self.shared)
            .expect("the registry is not shared yet")
            .limits = limits;
        self
    }
}

/// A join that was just started.
#[derive(Clone, Debug)]
pub struct StartedJoin {
    /// Identity of the join, used by every other call.
    pub join_id: String,

    /// Token the joining machine presents; its secret authorizes the join.
    pub token: JoinToken,

    /// Wall time after which the join expires and its token is refused.
    pub expires_at: SystemTime,
}

/// State of one join. Committed, cancelled, expired and failed are final.
#[derive(Clone, Debug, PartialEq)]
pub enum JoinStatus {
    /// No proposal is under review; the token waits for the joining machine.
    Waiting {
        /// How the latest attempt ended without a commit, if any.
        last_attempt: Option<JoinAttemptOutcome>,
    },

    /// A joining machine passed the dial-back check and waits for the operator's decision.
    AwaitingApproval {
        /// Identity of this proposal, which a decision names.
        proposal_id: String,

        /// What the operator decides on.
        review: Box<JoinReview>,
    },

    /// The site recorded the node, or a rejoining node's new addresses.
    Committed(JoinedNode),

    /// The join was cancelled, by the operator or because nobody followed it any more.
    Cancelled,

    /// The join's token expired before a node joined.
    Expired,

    /// The join ended without recording anything.
    Failed {
        /// Why, in words safe to show the operator.
        reason: String,
    },
}

impl JoinStatus {
    /// Reports whether the join has ended and its state no longer changes.
    pub fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Committed(_) | Self::Cancelled | Self::Expired | Self::Failed { .. }
        )
    }
}

/// How one attempt of a join ended without a commit; the token stays usable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JoinAttemptOutcome {
    /// The operator refused the proposal.
    Rejected,

    /// The attempt failed, for the reason given in words safe to show the operator.
    Failed(String),
}

/// Follows one join's state while it lives; the join counts as followed meanwhile.
pub struct JoinWatch {
    receiver: watch::Receiver<JoinStatus>,
    counted: bool,
    join: Arc<ActiveJoin>,
}

impl JoinWatch {
    /// Returns the join's current state.
    pub fn status(&mut self) -> JoinStatus {
        self.receiver.borrow_and_update().clone()
    }

    /// Waits for the next change of the join's state and returns it. Never completes once the
    /// join has ended.
    pub async fn changed(&mut self) -> JoinStatus {
        if self.receiver.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        self.status()
    }
}

impl Drop for JoinWatch {
    fn drop(&mut self) {
        if self.counted {
            self.join.lock().watchers -= 1;
            self.join.watchers_changed.notify_one();
        }
    }
}

/// Bounds on joins and on the attempts of each one.
#[derive(Clone, Copy, Debug)]
pub struct JoinLimits {
    /// Joins active at once; starting another fails until one ends.
    pub max_active_joins: usize,

    /// Attempts of one join whose dial-back failed before it fails. It bounds how often a token
    /// holder can make the manager's machine connect somewhere before approval.
    pub max_failed_dial_backs: u32,
}

impl Default for JoinLimits {
    fn default() -> Self {
        Self {
            max_active_joins: 8,
            max_failed_dial_backs: 3,
        }
    }
}

/// Why a join call was refused.
#[derive(Debug, thiserror::Error)]
pub enum JoinRegistryError {
    #[error("no such join is active")]
    NotFound,

    #[error("the proposal is not the one awaiting a decision")]
    StaleProposal,

    #[error("this proposal was already decided")]
    AlreadyDecided,

    #[error("too many joins are active; finish or cancel one first")]
    TooManyJoins,

    #[error("the manager's private listener is not serving yet")]
    NotListening,

    #[error("the join token could not be created: {0}")]
    Token(#[source] JoinTokenError),

    #[error("the site could not start a join: {0}")]
    Site(#[source] JoinSiteError),
}

/// The registry state its join service and management calls share.
pub(super) struct Registry {
    pub(super) identity: JoinHostIdentity,
    pub(super) site: Arc<dyn JoinSiteWriter>,
    pub(super) resolver: Arc<dyn HostResolver>,
    clock: Arc<dyn Clock>,
    timing: JoinTiming,
    pub(super) limits: JoinLimits,
    gate: JoinGate,
    /// Port tokens name, then the port offers name; the same one outside tests.
    private_port: OnceLock<(u16, u16)>,
    joins: Mutex<BTreeMap<String, Arc<ActiveJoin>>>,
}

impl Registry {
    /// Returns the private port offers name, which joined nodes keep dialing; known once any join
    /// started.
    pub(super) fn offered_private_port(&self) -> Option<u16> {
        self.private_port.get().map(|(_, offered)| *offered)
    }

    pub(super) fn timing(&self) -> &JoinTiming {
        &self.timing
    }

    /// Returns the active join whose token carries `presented`. A secret matching no join
    /// changes no join, since it can't be attributed to one: `connection` is closed instead, and
    /// the refusal is the one the private listener gives when no join is active, so it reveals
    /// nothing about the joins.
    pub(super) fn join_for_secret(
        &self,
        connection: &PrivateConnectionInfo,
        presented: &[u8],
    ) -> Result<Arc<ActiveJoin>, tonic::Status> {
        if let Some(join) = self
            .lock_joins()
            .values()
            .find(|join| !join.is_ended() && join.token.secret.matches(presented))
        {
            return Ok(Arc::clone(join));
        }
        warn!(
            connection = connection.id(),
            peer = %connection.peer_address(),
            "join request presented a secret matching no active join; closing its connection"
        );
        connection.close();
        Err(authentication_failure())
    }

    /// Reserves `node`'s identity, name and host for the attempt holding `ticket` of `join`,
    /// unless another join's attempt holds any of them.
    pub(super) fn reserve(
        &self,
        join: &ActiveJoin,
        ticket: u64,
        node: &SiteNode,
    ) -> Result<(), ReservationConflict> {
        let joins = self.lock_joins();
        for other in joins.values().filter(|other| other.id != join.id) {
            let progress = other.lock();
            let Some(reserved) = progress.slot.reserved() else {
                continue;
            };
            if reserved.display_name == node.display_name {
                return Err(ReservationConflict::Name);
            }
            if reserved.host == node.host {
                return Err(ReservationConflict::Host);
            }
            if reserved.node_id == node.node_id {
                return Err(ReservationConflict::Node);
            }
        }
        let mut progress = join.lock();
        match &mut progress.slot {
            Slot::Reviewing {
                ticket: current,
                reserved,
                ..
            } if *current == ticket => {
                *reserved = Some(node.clone());
                Ok(())
            }
            _ => Err(ReservationConflict::Abandoned),
        }
    }

    fn join(&self, join_id: &str) -> Result<Arc<ActiveJoin>, JoinRegistryError> {
        self.lock_joins()
            .get(join_id)
            .cloned()
            .ok_or(JoinRegistryError::NotFound)
    }

    fn active_join(&self, join_id: &str) -> Result<Arc<ActiveJoin>, JoinRegistryError> {
        Some(self.join(join_id)?)
            .filter(|join| !join.is_ended())
            .ok_or(JoinRegistryError::NotFound)
    }

    /// Ends the join when it expires or nobody follows it for the grace period, then forgets it
    /// once it stayed readable long enough.
    async fn supervise(self: Arc<Self>, join: Arc<ActiveJoin>) {
        let mut expiry = self.clock.sleep_until(join.expires_at);
        loop {
            let unwatched = join.lock().watchers == 0;
            let grace = async {
                if unwatched {
                    self.clock.sleep(self.timing.watcher_grace).await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                () = join.ended.cancelled() => break,
                () = &mut expiry => {
                    join.finish(JoinEnd::Expired);
                    join.ended.cancelled().await;
                    break;
                }
                () = grace => {
                    if join.lock().watchers == 0 {
                        info!(join_id = %join.id, "node join cancelled: nobody follows it");
                        join.finish(JoinEnd::Cancelled);
                        join.ended.cancelled().await;
                        break;
                    }
                }
                () = join.watchers_changed.notified() => {}
            }
        }
        self.clock.sleep(self.timing.ended_retention).await;
        let mut joins = self.lock_joins();
        if joins
            .get(&join.id)
            .is_some_and(|retained| Arc::ptr_eq(retained, &join))
        {
            joins.remove(&join.id);
        }
    }

    fn lock_joins(&self) -> MutexGuard<'_, BTreeMap<String, Arc<ActiveJoin>>> {
        self.joins.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Why an attempt cannot reserve what it would record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReservationConflict {
    /// Another join's attempt records the same node.
    Node,

    /// Another join's attempt records a node with the same name.
    Name,

    /// Another join's attempt records a node at the same host.
    Host,

    /// The attempt itself ended meanwhile.
    Abandoned,
}

/// One join the registry holds, active or ended.
pub(super) struct ActiveJoin {
    id: String,
    pub(super) token: JoinToken,
    expires_at: Instant,
    status: watch::Sender<JoinStatus>,
    progress: Mutex<JoinProgress>,
    watchers_changed: Notify,
    pub(super) ended: CancellationToken,
}

impl ActiveJoin {
    pub(super) fn id(&self) -> &str {
        &self.id
    }

    fn is_ended(&self) -> bool {
        self.ended.is_cancelled()
    }

    /// Claims the join's single attempt slot for one connection while its proposal is checked
    /// and reviewed. The slot frees itself when that connection closes.
    pub(super) fn claim_slot(
        self: &Arc<Self>,
        connection: &crate::private_transport::PrivateConnectionInfo,
    ) -> Result<SlotClaim, tonic::Status> {
        let (ticket, cancel) = {
            let mut progress = self.lock();
            if progress.ended {
                return Err(finished_status());
            }
            if !matches!(progress.slot, Slot::Free) {
                return Err(tonic::Status::failed_precondition(
                    "another machine is joining with this token; retry when it finishes",
                ));
            }
            progress.next_ticket += 1;
            let ticket = progress.next_ticket;
            let cancel = CancellationToken::new();
            progress.slot = Slot::Reviewing {
                ticket,
                connection: connection.id(),
                cancel: cancel.clone(),
                reserved: None,
                decision: None,
            };
            (ticket, cancel)
        };
        let join = Arc::clone(self);
        let watched = connection.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = watched.closed() => join.connection_closed(watched.id()),
                () = join.ended.cancelled() => {}
            }
        });
        Ok(SlotClaim {
            join: Arc::clone(self),
            ticket,
            cancel,
            armed: true,
            outcome: None,
        })
    }

    /// Counts one more failed dial-back toward the limit and returns the new count, or `None`
    /// once the join ended.
    pub(super) fn count_dial_back(&self) -> Option<u32> {
        let mut progress = self.lock();
        (!progress.ended).then(|| {
            progress.failed_dial_backs += 1;
            progress.failed_dial_backs
        })
    }

    /// Takes back a failed dial-back counted for one that succeeded.
    pub(super) fn refund_dial_back(&self) {
        let mut progress = self.lock();
        progress.failed_dial_backs = progress.failed_dial_backs.saturating_sub(1);
    }

    /// Shows the operator the proposal under `claim` and returns where its decision arrives.
    pub(super) fn await_decision(
        &self,
        claim: &SlotClaim,
        proposal_id: &str,
        review: JoinReview,
    ) -> Result<oneshot::Receiver<JoinDecision>, tonic::Status> {
        let mut progress = self.lock();
        let JoinProgress {
            slot,
            decided_proposal,
            ..
        } = &mut *progress;
        match slot {
            Slot::Reviewing {
                ticket, decision, ..
            } if *ticket == claim.ticket => {
                let (decide, decided) = oneshot::channel();
                *decision = Some(PendingDecision {
                    proposal_id: proposal_id.to_owned(),
                    decide,
                });
                *decided_proposal = None;
                self.status.send_replace(JoinStatus::AwaitingApproval {
                    proposal_id: proposal_id.to_owned(),
                    review: Box::new(review),
                });
                info!(join_id = %self.id, proposal_id, "node join awaits the operator's decision");
                Ok(decided)
            }
            _ => Err(abandoned_status()),
        }
    }

    /// Turns a reviewed claim into the outstanding offer, unless the attempt was abandoned
    /// meanwhile.
    pub(super) fn store_offer(
        &self,
        claim: &mut SlotClaim,
        offer: PendingOffer,
    ) -> Result<(), tonic::Status> {
        let mut progress = self.lock();
        match &progress.slot {
            Slot::Reviewing {
                ticket, connection, ..
            } if *ticket == claim.ticket => {
                progress.slot = Slot::Offered {
                    connection: *connection,
                    offer,
                };
                claim.armed = false;
                Ok(())
            }
            _ => Err(abandoned_status()),
        }
    }

    /// Moves the outstanding offer into the commit step when this connection owns it.
    pub(super) fn begin_commit(
        &self,
        connection: u64,
        offer_id: &str,
    ) -> Result<PendingOffer, tonic::Status> {
        let mut progress = self.lock();
        if progress.ended {
            return Err(finished_status());
        }
        match std::mem::replace(&mut progress.slot, Slot::Free) {
            Slot::Offered {
                connection: owner,
                offer,
            } if owner == connection && offer.offer_id == offer_id => {
                progress.slot = Slot::Committing {
                    node: offer.node.clone(),
                };
                Ok(offer)
            }
            other => {
                progress.slot = other;
                Err(tonic::Status::failed_precondition(
                    "no matching join offer is outstanding on this connection",
                ))
            }
        }
    }

    /// Ends the join with the recorded node.
    pub(super) fn committed(&self, joined: JoinedNode) {
        let mut progress = self.lock();
        if !progress.ended {
            self.end_locked(&mut progress, JoinEnd::Committed(joined));
        }
    }

    /// Reports a commit that recorded nothing: the join waits for another attempt, unless it
    /// was asked to end meanwhile.
    pub(super) fn commit_failed(&self, reason: &str) {
        let mut progress = self.lock();
        if progress.ended {
            return;
        }
        match progress.stop_after_commit.take() {
            Some(end) => self.end_locked(&mut progress, end),
            None => {
                progress.slot = Slot::Free;
                self.status.send_replace(JoinStatus::Waiting {
                    last_attempt: Some(JoinAttemptOutcome::Failed(reason.to_owned())),
                });
            }
        }
    }

    /// Ends the join after a commit whose outcome is unknown; the operator checks the site.
    pub(super) fn commit_uncertain(&self) {
        let mut progress = self.lock();
        if !progress.ended {
            self.end_locked(
                &mut progress,
                JoinEnd::Failed(
                    "the manager could not confirm that it recorded the node; check the site's machines"
                        .to_owned(),
                ),
            );
        }
    }

    /// Ends the join for `end`. A running commit reports its own outcome first; `end` applies
    /// only if that commit records nothing.
    pub(super) fn finish(&self, end: JoinEnd) {
        let mut progress = self.lock();
        if progress.ended {
            return;
        }
        if matches!(progress.slot, Slot::Committing { .. }) {
            progress.stop_after_commit.get_or_insert(end);
        } else {
            self.end_locked(&mut progress, end);
        }
    }

    /// Hands the operator's decision to the proposal `proposal_id` awaiting it.
    fn decide(&self, proposal_id: &str, decision: JoinDecision) -> Result<(), JoinRegistryError> {
        let mut progress = self.lock();
        let JoinProgress {
            slot,
            decided_proposal,
            ..
        } = &mut *progress;
        if let Slot::Reviewing {
            decision: pending, ..
        } = slot
            && pending
                .as_ref()
                .is_some_and(|pending| pending.proposal_id == proposal_id)
        {
            let pending = pending.take().expect("a pending decision");
            *decided_proposal = Some(proposal_id.to_owned());
            let _ = pending.decide.send(decision);
            info!(join_id = %self.id, proposal_id, ?decision, "operator decided on a node join");
            return Ok(());
        }
        if decided_proposal.as_deref() == Some(proposal_id) {
            return Err(JoinRegistryError::AlreadyDecided);
        }
        Err(JoinRegistryError::StaleProposal)
    }

    /// Frees the slot a closed connection held, unless its offer is being committed.
    fn connection_closed(&self, connection: u64) {
        let mut progress = self.lock();
        let owned = match &progress.slot {
            Slot::Reviewing {
                connection: owner, ..
            }
            | Slot::Offered {
                connection: owner, ..
            } => *owner == connection,
            Slot::Free | Slot::Committing { .. } => false,
        };
        if owned && !progress.ended {
            release_slot(&mut progress);
            self.status.send_replace(JoinStatus::Waiting {
                last_attempt: Some(JoinAttemptOutcome::Failed(
                    "the joining machine disconnected before the join completed".to_owned(),
                )),
            });
        }
    }

    fn end_locked(&self, progress: &mut JoinProgress, end: JoinEnd) {
        progress.ended = true;
        release_slot(progress);
        progress.gate = None;
        let status = end.into_status();
        info!(join_id = %self.id, status = ?status_kind(&status), "node join ended");
        self.status.send_replace(status);
        self.ended.cancel();
    }

    fn lock(&self) -> MutexGuard<'_, JoinProgress> {
        self.progress.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// How a join ends.
pub(super) enum JoinEnd {
    Committed(JoinedNode),
    Cancelled,
    Expired,
    Failed(String),
}

impl JoinEnd {
    fn into_status(self) -> JoinStatus {
        match self {
            Self::Committed(joined) => JoinStatus::Committed(joined),
            Self::Cancelled => JoinStatus::Cancelled,
            Self::Expired => JoinStatus::Expired,
            Self::Failed(reason) => JoinStatus::Failed { reason },
        }
    }
}

/// Frees the attempt slot of the join, and with it the claim's reservation, when the attempt
/// ends without an offer. The outcome recorded on the claim is shown to the operator.
pub(super) struct SlotClaim {
    join: Arc<ActiveJoin>,
    pub(super) ticket: u64,
    pub(super) cancel: CancellationToken,
    armed: bool,
    pub(super) outcome: Option<JoinAttemptOutcome>,
}

impl Drop for SlotClaim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut progress = self.join.lock();
        if !progress.ended
            && matches!(progress.slot, Slot::Reviewing { ticket, .. } if ticket == self.ticket)
        {
            release_slot(&mut progress);
            let outcome = self.outcome.take().unwrap_or_else(|| {
                JoinAttemptOutcome::Failed("the join attempt ended before an offer".to_owned())
            });
            self.join.status.send_replace(JoinStatus::Waiting {
                last_attempt: Some(outcome),
            });
        }
    }
}

#[derive(Default)]
struct JoinProgress {
    failed_dial_backs: u32,
    next_ticket: u64,
    slot: Slot,

    /// Proposal the operator decided on last, so a second decision on it is told so.
    decided_proposal: Option<String>,
    stop_after_commit: Option<JoinEnd>,
    ended: bool,
    watchers: usize,

    /// Keeps the private listener admitting joining machines while the join is active.
    gate: Option<OpenJoinGate>,
}

/// The single attempt a join handles at a time.
#[derive(Default)]
enum Slot {
    #[default]
    Free,

    Reviewing {
        ticket: u64,
        connection: u64,
        cancel: CancellationToken,

        /// The node the attempt would record, once admitted.
        reserved: Option<SiteNode>,
        decision: Option<PendingDecision>,
    },

    Offered {
        connection: u64,
        offer: PendingOffer,
    },

    Committing {
        node: SiteNode,
    },
}

impl Slot {
    /// Returns the node this slot's attempt reserves, if any.
    fn reserved(&self) -> Option<&SiteNode> {
        match self {
            Self::Reviewing { reserved, .. } => reserved.as_ref(),
            Self::Offered { offer, .. } => Some(&offer.node),
            Self::Committing { node } => Some(node),
            Self::Free => None,
        }
    }
}

/// A proposal waiting for the operator's decision.
struct PendingDecision {
    proposal_id: String,
    decide: oneshot::Sender<JoinDecision>,
}

/// Frees the slot, cancelling a review in progress.
fn release_slot(progress: &mut JoinProgress) {
    if let Slot::Reviewing { cancel, .. } = std::mem::take(&mut progress.slot) {
        cancel.cancel();
    }
}

fn status_kind(status: &JoinStatus) -> &'static str {
    match status {
        JoinStatus::Waiting { .. } => "waiting",
        JoinStatus::AwaitingApproval { .. } => "awaiting_approval",
        JoinStatus::Committed(_) => "committed",
        JoinStatus::Cancelled => "cancelled",
        JoinStatus::Expired => "expired",
        JoinStatus::Failed { .. } => "failed",
    }
}

/// The join has ended; nothing more will be recorded with this token.
pub(super) fn finished_status() -> tonic::Status {
    tonic::Status::failed_precondition("this join token is no longer accepted")
}

pub(super) fn abandoned_status() -> tonic::Status {
    tonic::Status::aborted("the join was abandoned before an offer was made")
}
