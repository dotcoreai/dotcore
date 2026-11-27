//! The re-scan management surface: an operator starts a re-scan invitation for one declared node,
//! follows it, and decides on the replacement the node proposed.
//!
//! It projects the rescan registry onto the wire. A watch sends the re-scan's current state first,
//! then every change, a heartbeat after a quiet interval, and ends after a final state. The
//! operator's decision names the exact proposal it was made on, so a decision that no longer
//! matches the proposal under review is refused.

use std::{pin::Pin, sync::Arc, time::Duration};

use dotcore_domain::clock::Clock;
use dotcore_proto::{
    dotcore::site::v1::{
        ApproveRescanRequest, ApproveRescanResponse, CancelRescanRequest, CancelRescanResponse,
        RejectRescanRequest, RejectRescanResponse, RescanAttemptOutcome as ProtoAttemptOutcome,
        RescanAwaitingApproval as AwaitingReview, RescanCommitted as ProtoCommitted, RescanFailed,
        RescanNoOp as ProtoNoOp, RescanProposal, RescanState, RescanWaiting, StartRescanRequest,
        StartRescanResponse, WatchRescanRequest, WatchRescanResponse, rescan_attempt_outcome,
        rescan_state,
        site_rescan_service_server::{SiteRescanService, SiteRescanServiceServer},
        watch_rescan_response,
    },
    google::protobuf::{Empty, Timestamp},
};
use futures_util::{Stream, stream};
use tonic::{Request, Response, Status};

use super::registry::{
    RescanAttempt, RescanAttemptOutcome, RescanRegistry, RescanRegistryError, RescanReview,
    RescanStatus, RescanWatch,
};

/// The management service over one running site manager's rescan registry.
pub struct RescanManagementService {
    registry: RescanRegistry,
    clock: Arc<dyn Clock>,
    heartbeat_interval: Duration,
}

impl RescanManagementService {
    /// Serves `registry`, sending a heartbeat on a watch quiet for `heartbeat_interval` on `clock`.
    pub fn new(
        registry: RescanRegistry,
        clock: Arc<dyn Clock>,
        heartbeat_interval: Duration,
    ) -> Self {
        Self {
            registry,
            clock,
            heartbeat_interval,
        }
    }

    pub fn into_server(self) -> SiteRescanServiceServer<Self> {
        SiteRescanServiceServer::new(self)
    }
}

type RescanStateStream = Pin<Box<dyn Stream<Item = Result<WatchRescanResponse, Status>> + Send>>;

#[tonic::async_trait]
impl SiteRescanService for RescanManagementService {
    type WatchRescanStream = RescanStateStream;

    async fn start_rescan(
        &self,
        request: Request<StartRescanRequest>,
    ) -> Result<Response<StartRescanResponse>, Status> {
        let started = self
            .registry
            .start(&request.into_inner().node_id)
            .await
            .map_err(registry_status)?;
        Ok(Response::new(StartRescanResponse {
            rescan_id: started.rescan_id,
            expires_at: Some(timestamp(started.expires_at)),
        }))
    }

    async fn watch_rescan(
        &self,
        request: Request<WatchRescanRequest>,
    ) -> Result<Response<Self::WatchRescanStream>, Status> {
        let watch = self
            .registry
            .watch(&request.into_inner().rescan_id)
            .map_err(registry_status)?;
        Ok(Response::new(state_stream(
            watch,
            Arc::clone(&self.clock),
            self.heartbeat_interval,
        )))
    }

    async fn approve_rescan(
        &self,
        request: Request<ApproveRescanRequest>,
    ) -> Result<Response<ApproveRescanResponse>, Status> {
        let request = request.into_inner();
        self.registry
            .approve(&request.rescan_id, &request.proposal_id)
            .map_err(registry_status)?;
        Ok(Response::new(ApproveRescanResponse {}))
    }

    async fn reject_rescan(
        &self,
        request: Request<RejectRescanRequest>,
    ) -> Result<Response<RejectRescanResponse>, Status> {
        let request = request.into_inner();
        self.registry
            .reject(&request.rescan_id, &request.proposal_id)
            .map_err(registry_status)?;
        Ok(Response::new(RejectRescanResponse {}))
    }

    async fn cancel_rescan(
        &self,
        request: Request<CancelRescanRequest>,
    ) -> Result<Response<CancelRescanResponse>, Status> {
        self.registry
            .cancel(&request.into_inner().rescan_id)
            .map_err(registry_status)?;
        Ok(Response::new(CancelRescanResponse {}))
    }
}

/// Sends the re-scan's current state, then each change or a heartbeat after a quiet interval, and
/// ends after a final state.
fn state_stream(
    watch: RescanWatch,
    clock: Arc<dyn Clock>,
    heartbeat_interval: Duration,
) -> RescanStateStream {
    Box::pin(stream::unfold(Some((watch, true)), move |next| {
        let clock = Arc::clone(&clock);
        async move {
            let (mut watch, first) = next?;
            let status = if first {
                watch.status()
            } else {
                tokio::select! {
                    status = watch.changed() => status,
                    () = clock.sleep(heartbeat_interval) => {
                        let heartbeat = WatchRescanResponse {
                            event: Some(watch_rescan_response::Event::Heartbeat(Empty {})),
                        };
                        return Some((Ok(heartbeat), Some((watch, false))));
                    }
                }
            };
            let message = WatchRescanResponse {
                event: Some(watch_rescan_response::Event::State(rescan_state(&status))),
            };
            let next = (!status.is_final()).then_some((watch, false));
            Some((Ok(message), next))
        }
    }))
}

fn rescan_state(status: &RescanStatus) -> RescanState {
    let state = match status {
        RescanStatus::Waiting { last_attempt } => rescan_state::State::Waiting(RescanWaiting {
            last_attempt: last_attempt.as_ref().map(attempt),
        }),
        RescanStatus::AwaitingApproval {
            proposal_id,
            review,
        } => rescan_state::State::AwaitingApproval(AwaitingReview {
            proposal_id: proposal_id.clone(),
            proposal: Some(proposal(review)),
        }),
        RescanStatus::Committed(committed) => rescan_state::State::Committed(ProtoCommitted {
            node_id: committed.node_id.clone(),
            targets: committed.targets.clone(),
            restart_required: true,
        }),
        RescanStatus::NoOp { node_id } => rescan_state::State::NoOp(ProtoNoOp {
            node_id: node_id.clone(),
        }),
        RescanStatus::Cancelled => rescan_state::State::Cancelled(Empty {}),
        RescanStatus::Expired => rescan_state::State::Expired(Empty {}),
        RescanStatus::Failed { reason } => rescan_state::State::Failed(RescanFailed {
            reason: reason.clone(),
        }),
    };
    RescanState { state: Some(state) }
}

/// Projects what the operator decides on: the node, and its whole replacement classified against
/// the committed targets. Deployments placed on a changed or removed target made the replacement
/// refuse before it was offered, so the list is always empty on a proposal that reached review.
fn proposal(review: &RescanReview) -> RescanProposal {
    RescanProposal {
        node_id: review.node_id.clone(),
        display_name: review.display_name.clone(),
        added: review.classification.added.clone(),
        changed: review.classification.changed.clone(),
        removed: review.classification.removed.clone(),
        unchanged: review.classification.unchanged.clone(),
        deployments: Vec::new(),
    }
}

fn attempt(attempt: &RescanAttempt) -> ProtoAttemptOutcome {
    ProtoAttemptOutcome {
        outcome: Some(match &attempt.outcome {
            RescanAttemptOutcome::Refused => rescan_attempt_outcome::Outcome::Rejected(Empty {}),
            RescanAttemptOutcome::Failed(reason) => {
                rescan_attempt_outcome::Outcome::Failed(reason.clone())
            }
        }),
        at: Some(timestamp(attempt.at)),
    }
}

fn registry_status(error: RescanRegistryError) -> Status {
    match error {
        RescanRegistryError::NotFound => Status::not_found(error.to_string()),
        RescanRegistryError::StaleProposal
        | RescanRegistryError::AlreadyDecided
        | RescanRegistryError::CommitInProgress => Status::failed_precondition(error.to_string()),
        RescanRegistryError::AlreadyOpen => Status::failed_precondition(error.to_string()),
        RescanRegistryError::TooManyRescans => Status::resource_exhausted(error.to_string()),
        RescanRegistryError::NotDeclared { .. } => Status::not_found(error.to_string()),
        RescanRegistryError::ManagerNode { .. } => Status::failed_precondition(error.to_string()),
        RescanRegistryError::Identity => Status::internal(error.to_string()),
        RescanRegistryError::Site(source) => {
            Status::unavailable(format!("{source}; no re-scan was started"))
        }
    }
}

fn timestamp(at: std::time::SystemTime) -> Timestamp {
    let at = at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    Timestamp {
        seconds: at.as_secs() as i64,
        nanos: at.subsec_nanos() as i32,
    }
}
