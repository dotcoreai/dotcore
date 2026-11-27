//! The private rescan handshake the site manager hosts: one declared node proves the pinned
//! identity it already holds, proposes its whole replacement target set, and waits while the
//! operator reviews that exact change.
//!
//! The private listener authenticates the connection against the live pins before any request
//! reaches this service, so the host binds the proposal to that exact identity and to the
//! certificate the node presented, derives the whole-node base itself from durable state, and
//! validates the replacement through the shared derive: the deployment guard and the whole-site
//! check refuse a bad replacement before the operator ever sees it. Nothing is recorded until the
//! operator decides on the proposal the review showed. The commit re-checks the pin, the base and
//! the exact replacement and fails closed on anything that differs, and a replacement the durable
//! site already declares writes nothing rather than rolling anything back.

use std::sync::Arc;

use dotcore_proto::dotcore::{
    node::v1::{
        ProposeRescanRequest, ProposeRescanResponse, RescanCommitted as RecordedReplacement,
        RescanNoOp as WholeProjectionUnchanged, node_rescan_service_server::NodeRescanService,
        propose_rescan_response::Outcome,
    },
    site::v1::Site,
};
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use super::commit::RescanCommitError;
use super::registry::{
    CommittedReplacement, ProposalClaim, RescanAttemptOutcome, RescanDecision, Rescans,
};
use super::{RescanError, derive_replacement, rescan_base};
use crate::{
    node::folder::CertificateFingerprint,
    private_transport::{
        AuthenticatedMachineIdentity, PrivateConnectionInfo, PrivateNodeRole,
        authentication_failure,
    },
};

/// The rescan service of one running site manager, hosting every open rescan invitation.
pub struct RescanService {
    rescans: Arc<super::registry::Rescans>,
}

impl RescanService {
    pub(super) fn new(rescans: Arc<super::registry::Rescans>) -> Self {
        Self { rescans }
    }
}

#[tonic::async_trait]
impl NodeRescanService for RescanService {
    /// Answers one node's rescan invitation with its whole replacement target set. Blocks while
    /// the operator reviews and decides, then answers with the committed projection or the reason
    /// the replacement was refused.
    async fn propose_rescan(
        &self,
        request: Request<ProposeRescanRequest>,
    ) -> Result<Response<ProposeRescanResponse>, Status> {
        let identity = request
            .extensions()
            .get::<AuthenticatedMachineIdentity>()
            .cloned()
            .ok_or_else(authentication_failure)?;
        let connection = request
            .extensions()
            .get::<PrivateConnectionInfo>()
            .cloned()
            .ok_or_else(|| Status::internal("rescan connection information is missing"))?;
        let presented = connection
            .peer_certificate()
            .ok_or_else(authentication_failure)?
            .to_vec();
        let proposal = request.into_inner();

        // The node folder's own certificate must be the identity this connection proved: a folder
        // re-read under custody that names another certificate is not this node's folder.
        if proposal.node_certificate_der != presented {
            return Err(Status::failed_precondition(
                "the node folder's certificate is not the identity this connection proved",
            ));
        }
        if identity.role != PrivateNodeRole::Worker || identity.node_id != proposal.node_id {
            return Err(authentication_failure());
        }
        let Some(observation) = proposal.observation.clone() else {
            return Err(Status::failed_precondition(
                "the replacement carries no observation configuration",
            ));
        };

        // A node that lost the answer to a proposal the site already recorded is handed the same
        // committed replacement once, before anything is derived or reviewed again.
        if let Some(committed) = self
            .rescans
            .replay_committed(&proposal.node_id, &proposal.targets)
        {
            info!(node_id = %proposal.node_id, "re-scan replacement replayed to its node");
            return Ok(Response::new(committed_response(committed)));
        }

        let site = read_site(&self.rescans).await?;
        let declared = site
            .nodes
            .iter()
            .find(|node| node.node_id == proposal.node_id);
        let Some(declared) = declared else {
            return Err(Status::not_found(format!(
                "node {:?} is not declared in this site",
                proposal.node_id
            )));
        };
        if declared.site_manager.is_some() {
            return Err(Status::failed_precondition(format!(
                "node {:?} is this site's manager; re-scan its targets with the command on this machine",
                proposal.node_id
            )));
        }
        let Some(rescan) = self.rescans.open_rescan_for(&proposal.node_id) else {
            return Err(Status::failed_precondition(
                "no re-scan is open for this node; start one before running the command again",
            ));
        };

        let rules = self.rescans.declaration_rules();
        let base = rescan_base(&site, &proposal.node_id).map_err(derive_status)?;
        let replacement = derive_replacement(
            &site,
            &rules,
            &proposal.node_id,
            proposal.targets.clone(),
            Some(&base),
        )
        .map_err(|error| {
            rescan.answer_failed(error.to_string());
            derive_status(error)
        })?;

        // The whole projection equals what the site already declares and what this node installs:
        // nothing is written anywhere and the node needs no restart.
        if replacement.is_noop() && observation == base.observation {
            info!(node_id = %proposal.node_id, "node re-scan found no change to install");
            rescan.answer_no_op()?;
            return Ok(Response::new(ProposeRescanResponse {
                outcome: Some(Outcome::NoOp(WholeProjectionUnchanged {
                    node_id: proposal.node_id,
                })),
            }));
        }

        let mut claim = rescan.review(base, replacement, presented)?;
        let waited = tokio::select! {
            decision = claim.await_decision() => match decision {
                Some(RescanDecision::Allowed) => Waited::Allowed,
                Some(RescanDecision::Refused) => Waited::Refused,
                None => Waited::RescanEnded,
            },
            () = connection.closed() => Waited::ConnectionLost,
            () = rescan.ended.cancelled() => Waited::RescanEnded,
        };
        match waited {
            Waited::Refused => {
                info!(node_id = %proposal.node_id, "node re-scan proposal refused");
                claim.finish(Some(RescanAttemptOutcome::Refused));
                Err(Status::failed_precondition(
                    "the operator refused this replacement",
                ))
            }
            Waited::ConnectionLost => {
                warn!(node_id = %proposal.node_id, "node closed its re-scan connection before the decision");
                claim.finish(None);
                Err(Status::cancelled(
                    "the node closed the re-scan connection before the decision",
                ))
            }
            Waited::RescanEnded => {
                claim.finish(None);
                Err(Status::aborted("this re-scan ended before the decision"))
            }
            Waited::Allowed => {
                // The node's connection dropping drops this handler, but a write that may already
                // have landed must still record its outcome, so the node's retry replays it. The
                // commit runs in its own task, which the handler only waits on.
                let committed = tokio::spawn(commit_and_record(Arc::clone(&self.rescans), claim))
                    .await
                    .map_err(|_| Status::internal("the re-scan commit stopped unexpectedly"))??;
                Ok(Response::new(committed_response(committed)))
            }
        }
    }
}

/// Commits the approved proposal `claim` and records the outcome on its re-scan, so the re-scan
/// ends committed or failed whether or not its node is still waiting for the answer.
async fn commit_and_record(
    rescans: Arc<Rescans>,
    claim: ProposalClaim,
) -> Result<CommittedReplacement, Status> {
    match commit(&rescans, &claim).await {
        Ok(committed) => {
            claim.commit(committed.clone());
            info!(node_id = %committed.node_id, targets = committed.targets.len(), "node re-scan replacement recorded");
            Ok(committed)
        }
        Err(status) => {
            // A refusal that named its own reason already ended the re-scan; any other failure
            // ends it with the answer the node gets, so no failure leaves it awaiting approval.
            warn!(node_id = %claim.base.node.node_id, reason = status.message(), "re-scan commit refused");
            claim.fail(status.message());
            Err(status)
        }
    }
}

/// Re-checks the pin, the whole-node base and the exact replacement against durable state, then
/// records the replacement. A replacement the site already declares is reported committed without
/// another write, and a write whose outcome stays uncertain is answered only by comparing the
/// durable declaration: a recorded replacement is never rolled back.
async fn commit(rescans: &Rescans, claim: &ProposalClaim) -> Result<CommittedReplacement, Status> {
    let node_id = claim.base.node.node_id.clone();
    let pin = {
        let writer = Arc::clone(&rescans.site);
        let certificate = claim.certificate().to_vec();
        let pinned_node = node_id.clone();
        blocking(move || {
            writer
                .node_pin(&pinned_node)
                .map(|pin| pin == Some(CertificateFingerprint::for_certificate(&certificate)))
        })
        .await
        .map_err(|_| Status::internal("the re-scan commit stopped unexpectedly"))?
        .map_err(commit_status)?
    };
    if !pin {
        claim.fail("the node's certificate no longer matches its pin");
        return Err(Status::failed_precondition(
            "the node's certificate no longer matches its pin",
        ));
    }

    let site = read_site(rescans).await?;
    let rules = rescans.declaration_rules();
    let exact = derive_replacement(
        &site,
        &rules,
        &node_id,
        claim.replacement.targets().to_vec(),
        Some(&claim.base),
    )
    .map_err(|error| {
        claim.fail(&error.to_string());
        derive_status(error)
    })?;
    if exact != claim.replacement {
        claim.fail("the site's declaration changed while the replacement awaited approval");
        return Err(Status::failed_precondition(
            "the site's declaration changed while the replacement awaited approval; run the re-scan again",
        ));
    }

    let committed = CommittedReplacement {
        node_id: node_id.clone(),
        targets: claim.replacement.targets().to_owned(),
        observation: claim.replacement.observation().clone(),
        observation_configuration_sha256: claim
            .replacement
            .observation_configuration_sha256()
            .to_owned(),
    };
    if exact.is_noop() {
        // The durable site already declares exactly this replacement: the node installs it and
        // restarts, and nothing is written to the site again.
        return Ok(committed);
    }

    let writer = Arc::clone(&rescans.site);
    let base = claim.base.clone();
    let replacement = claim.replacement.clone();
    let written = blocking(move || writer.commit_replacement(&base, &replacement))
        .await
        .map_err(|_| {
            Status::internal("the re-scan commit stopped unexpectedly; nothing was written")
        })?;
    match written {
        Ok(()) => Ok(committed),
        Err(error @ RescanCommitError::OutcomeUncertain(_)) => {
            // The write may have landed. Compare the durable declaration rather than guessing,
            // and never roll a recorded replacement back.
            warn!(node_id = %node_id, error = %error, "re-scan write outcome uncertain; comparing the durable declaration");
            let site = read_site(rescans).await?;
            let recorded = rescan_base(&site, &node_id)
                .map(|base| {
                    base.targets == committed.targets && base.observation == committed.observation
                })
                .map_err(derive_status)?;
            if recorded {
                info!(node_id = %node_id, "re-scan replacement is durable despite the uncertain write");
                return Ok(committed);
            }
            claim.fail("the replacement write had no known outcome");
            Err(Status::internal(
                "the site could not confirm whether it recorded the replacement; nothing else changed, so run the re-scan again once the storage cause is resolved",
            ))
        }
        Err(error) => {
            claim.fail(&error.to_string());
            Err(commit_status(error))
        }
    }
}

/// How a proposal waiting for the operator's decision ended.
enum Waited {
    Allowed,
    Refused,
    ConnectionLost,
    RescanEnded,
}

fn committed_response(committed: CommittedReplacement) -> ProposeRescanResponse {
    // The node installs the committed projection, so it restarts before serving under it.
    let restart_required = true;
    ProposeRescanResponse {
        outcome: Some(Outcome::Committed(RecordedReplacement {
            node_id: committed.node_id,
            targets: committed.targets,
            observation: Some(committed.observation),
            observation_configuration_sha256: committed.observation_configuration_sha256,
            restart_required,
        })),
    }
}

/// Reads the running site off the request thread.
async fn read_site(rescans: &super::registry::Rescans) -> Result<Site, Status> {
    let writer = Arc::clone(&rescans.site);
    blocking(move || writer.site())
        .await
        .map_err(|_| Status::internal("the site read stopped unexpectedly"))?
        .map_err(commit_status)
}

/// Maps a refusal of the replacement to the code the contract names: an unknown node, and
/// everything else — an invalid replacement, a stale base, the deployment guard — as a failed
/// precondition that leaves the node and the site unchanged.
fn derive_status(error: RescanError) -> Status {
    match error {
        RescanError::NodeNotDeclared { .. } => Status::not_found(error.to_string()),
        RescanError::StaleBase => Status::failed_precondition(format!(
            "{error}; run the re-scan again to approve the current base"
        )),
        error => Status::failed_precondition(error.to_string()),
    }
}

/// Maps a site write failure to a status: the site refusing the replacement is a failed
/// precondition, an uncertain write says so plainly, and anything else is an internal failure.
fn commit_status(error: RescanCommitError) -> Status {
    match error {
        RescanCommitError::Rejected(source) => {
            Status::failed_precondition(format!("the site refused this replacement: {source}"))
        }
        RescanCommitError::Failed(source) => Status::internal(format!(
            "the site could not record the replacement; nothing was written: {source}"
        )),
        RescanCommitError::OutcomeUncertain(source) => Status::internal(format!(
            "the site may or may not have recorded the replacement: {source}"
        )),
    }
}

pub(super) fn random_id() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

pub(super) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Status::internal("re-scan task failed"))
}
