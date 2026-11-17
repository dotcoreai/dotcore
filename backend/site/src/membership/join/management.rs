//! The join management API: starts a join and hands out its token, streams each join's state,
//! and takes the operator's approve, reject and cancel decisions.
//!
//! It projects the join registry onto the wire. A watch sends the join's current state first,
//! then every change, a heartbeat after a quiet interval, and ends once the join has ended.

use std::{pin::Pin, sync::Arc, time::Duration};

use dotcore_domain::clock::Clock;
use dotcore_proto::{
    dotcore::site::v1::{
        ApproveJoinRequest, ApproveJoinResponse, CancelJoinRequest, CancelJoinResponse,
        ExecutionTarget, ExecutionTargetWorkloadKind, JoinAttemptOutcome as ProtoAttemptOutcome,
        JoinAwaitingApproval, JoinCommitted, JoinFailed, JoinHostSource as ProtoHostSource,
        JoinNewNode, JoinProposal as ProtoProposal, JoinProposedTarget, JoinRejoin, JoinState,
        JoinTargetRuntime, JoinWaiting, NativeWorkloadLifetime, RejectJoinRequest,
        RejectJoinResponse, StartJoinRequest, StartJoinResponse, WatchJoinRequest,
        WatchJoinResponse, application_runtime_binding, execution_target, join_attempt_outcome,
        join_proposal, join_state, model_runtime_binding,
        site_join_service_server::{SiteJoinService, SiteJoinServiceServer},
        watch_join_response,
    },
    google::protobuf::{Empty, Timestamp},
};
use futures_util::{Stream, stream};
use tonic::{Request, Response, Status};

use super::{
    JoinAttemptOutcome, JoinHostSource, JoinRegistry, JoinRegistryError, JoinReview, JoinStatus,
    JoinWatch, ReviewedJoin,
};

/// The management service over the site manager's join registry.
pub struct JoinManagementService {
    registry: JoinRegistry,
    clock: Arc<dyn Clock>,
    heartbeat_interval: Duration,
}

impl JoinManagementService {
    /// Serves `registry`, sending a heartbeat on a watch quiet for `heartbeat_interval` on
    /// `clock`.
    pub fn new(
        registry: JoinRegistry,
        clock: Arc<dyn Clock>,
        heartbeat_interval: Duration,
    ) -> Self {
        Self {
            registry,
            clock,
            heartbeat_interval,
        }
    }

    pub fn into_server(self) -> SiteJoinServiceServer<Self> {
        SiteJoinServiceServer::new(self)
    }
}

type JoinStateStream = Pin<Box<dyn Stream<Item = Result<WatchJoinResponse, Status>> + Send>>;

#[tonic::async_trait]
impl SiteJoinService for JoinManagementService {
    type WatchJoinStream = JoinStateStream;

    async fn start_join(
        &self,
        _request: Request<StartJoinRequest>,
    ) -> Result<Response<StartJoinResponse>, Status> {
        let started = self.registry.start().await.map_err(registry_status)?;
        Ok(Response::new(StartJoinResponse {
            join_id: started.join_id,
            token: started.token.encode(),
            expires_at: Some(timestamp(started.expires_at)),
        }))
    }

    async fn watch_join(
        &self,
        request: Request<WatchJoinRequest>,
    ) -> Result<Response<Self::WatchJoinStream>, Status> {
        let watch = self
            .registry
            .watch(&request.into_inner().join_id)
            .map_err(registry_status)?;
        Ok(Response::new(state_stream(
            watch,
            Arc::clone(&self.clock),
            self.heartbeat_interval,
        )))
    }

    async fn approve_join(
        &self,
        request: Request<ApproveJoinRequest>,
    ) -> Result<Response<ApproveJoinResponse>, Status> {
        let request = request.into_inner();
        self.registry
            .approve(&request.join_id, &request.proposal_id)
            .map_err(registry_status)?;
        Ok(Response::new(ApproveJoinResponse {}))
    }

    async fn reject_join(
        &self,
        request: Request<RejectJoinRequest>,
    ) -> Result<Response<RejectJoinResponse>, Status> {
        let request = request.into_inner();
        self.registry
            .reject(&request.join_id, &request.proposal_id)
            .map_err(registry_status)?;
        Ok(Response::new(RejectJoinResponse {}))
    }

    async fn cancel_join(
        &self,
        request: Request<CancelJoinRequest>,
    ) -> Result<Response<CancelJoinResponse>, Status> {
        self.registry
            .cancel(&request.into_inner().join_id)
            .map_err(registry_status)?;
        Ok(Response::new(CancelJoinResponse {}))
    }
}

/// Sends the join's current state, then each change or a heartbeat after a quiet interval, and
/// ends after a final state.
fn state_stream(
    watch: JoinWatch,
    clock: Arc<dyn Clock>,
    heartbeat_interval: Duration,
) -> JoinStateStream {
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
                        let heartbeat = WatchJoinResponse {
                            event: Some(watch_join_response::Event::Heartbeat(Empty {})),
                        };
                        return Some((Ok(heartbeat), Some((watch, false))));
                    }
                }
            };
            let message = WatchJoinResponse {
                event: Some(watch_join_response::Event::State(join_state(&status))),
            };
            let next = (!status.is_final()).then_some((watch, false));
            Some((Ok(message), next))
        }
    }))
}

/// Projects a join's state onto the wire.
fn join_state(status: &JoinStatus) -> JoinState {
    let state = match status {
        JoinStatus::Waiting { last_attempt } => join_state::State::Waiting(JoinWaiting {
            last_attempt: last_attempt.as_ref().map(|outcome| ProtoAttemptOutcome {
                outcome: Some(match outcome {
                    JoinAttemptOutcome::Rejected => {
                        join_attempt_outcome::Outcome::Rejected(Empty {})
                    }
                    JoinAttemptOutcome::Failed(reason) => {
                        join_attempt_outcome::Outcome::Failed(reason.clone())
                    }
                }),
            }),
        }),
        JoinStatus::AwaitingApproval {
            proposal_id,
            review,
        } => join_state::State::AwaitingApproval(JoinAwaitingApproval {
            proposal_id: proposal_id.clone(),
            proposal: Some(proposal(review)),
        }),
        JoinStatus::Committed(joined) => join_state::State::Committed(JoinCommitted {
            node_id: joined.node_id.clone(),
        }),
        JoinStatus::Cancelled => join_state::State::Cancelled(Empty {}),
        JoinStatus::Expired => join_state::State::Expired(Empty {}),
        JoinStatus::Failed { reason } => join_state::State::Failed(JoinFailed {
            reason: reason.clone(),
        }),
    };
    JoinState { state: Some(state) }
}

/// Projects what the operator reviews onto the wire.
fn proposal(review: &JoinReview) -> ProtoProposal {
    let (display_name, kind) = match &review.request {
        ReviewedJoin::New(proposal) => (
            proposal.display_name.clone(),
            join_proposal::Kind::NewNode(JoinNewNode {
                targets: review.targets.iter().map(proposed_target).collect(),
            }),
        ),
        ReviewedJoin::Rejoin(rejoin) => (
            rejoin.declared.display_name.clone(),
            join_proposal::Kind::Rejoin(JoinRejoin {
                previous_host: rejoin.declared.host.clone(),
                previous_manager_host: rejoin.declared.manager_host.clone(),
                manager_host: rejoin.rejoined.manager_host.clone(),
                deployments: u32::try_from(rejoin.deployments).unwrap_or(u32::MAX),
            }),
        ),
    };
    let private_port = match &review.request {
        ReviewedJoin::New(proposal) => proposal.private_port,
        ReviewedJoin::Rejoin(rejoin) => rejoin.private_port,
    };
    ProtoProposal {
        node_id: review.node_id.clone(),
        display_name,
        host: review.host.to_string(),
        host_source: match review.host_source {
            JoinHostSource::Proposed => ProtoHostSource::Proposed,
            JoinHostSource::Observed => ProtoHostSource::Observed,
            JoinHostSource::Declared => ProtoHostSource::Declared,
        } as i32,
        peer_address: review.peer_address.to_string(),
        host_differs_from_peer: review.host_differs_from_peer,
        private_port: u32::from(private_port),
        key_fingerprint: review.csr_key_fingerprint.as_str().to_owned(),
        kind: Some(kind),
    }
}

/// Projects one execution target a new node proposes.
fn proposed_target(target: &ExecutionTarget) -> JoinProposedTarget {
    let unspecified = NativeWorkloadLifetime::Unspecified as i32;
    let (workload_kind, runtime, lifetime) = match &target.runtime_binding {
        Some(execution_target::RuntimeBinding::ModelRuntime(binding)) => {
            let (runtime, lifetime) = match &binding.adapter {
                Some(model_runtime_binding::Adapter::SimulatedRuntime(_)) => {
                    (JoinTargetRuntime::Simulated, unspecified)
                }
                Some(model_runtime_binding::Adapter::DockerRuntime(_)) => {
                    (JoinTargetRuntime::Nvidia, unspecified)
                }
                Some(model_runtime_binding::Adapter::NativeRuntime(native)) => {
                    (JoinTargetRuntime::Apple, native.workload_lifetime)
                }
                None => (JoinTargetRuntime::Unspecified, unspecified),
            };
            (ExecutionTargetWorkloadKind::Model, runtime, lifetime)
        }
        Some(execution_target::RuntimeBinding::ApplicationRuntime(binding)) => {
            let runtime = match &binding.adapter {
                Some(application_runtime_binding::Adapter::DockerRuntime(_)) => {
                    JoinTargetRuntime::Docker
                }
                Some(application_runtime_binding::Adapter::SimulatedRuntime(_)) => {
                    JoinTargetRuntime::Simulated
                }
                None => JoinTargetRuntime::Unspecified,
            };
            (
                ExecutionTargetWorkloadKind::Application,
                runtime,
                unspecified,
            )
        }
        None => (
            ExecutionTargetWorkloadKind::Unspecified,
            JoinTargetRuntime::Unspecified,
            unspecified,
        ),
    };
    JoinProposedTarget {
        workload_kind: workload_kind as i32,
        runtime: runtime as i32,
        platform_label: target
            .platform
            .as_ref()
            .map(|platform| format!("{}/{}", platform.operating_system, platform.architecture))
            .unwrap_or_default(),
        accelerator_device_ids: target.accelerator_device_ids.clone(),
        native_workload_lifetime: lifetime,
    }
}

/// Projects a refused join call into a safe status.
fn registry_status(error: JoinRegistryError) -> Status {
    match &error {
        JoinRegistryError::NotFound => Status::not_found(error.to_string()),
        JoinRegistryError::StaleProposal | JoinRegistryError::AlreadyDecided => {
            Status::failed_precondition(error.to_string())
        }
        JoinRegistryError::TooManyJoins => Status::resource_exhausted(error.to_string()),
        JoinRegistryError::NotListening => Status::unavailable(error.to_string()),
        JoinRegistryError::Token(_) | JoinRegistryError::Site(_) => {
            tracing::warn!(error = %error, "node join could not start");
            Status::internal("the manager could not start a join")
        }
    }
}

fn timestamp(time: std::time::SystemTime) -> Timestamp {
    let since_epoch = time
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: i64::try_from(since_epoch.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(since_epoch.subsec_nanos()).unwrap_or(0),
    }
}
