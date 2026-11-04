//! Serves deployment runtime logs on a node's private listener to the authenticated site
//! manager, for deployments assigned in the node's installed view.
//!
//! Each request names the manager session, the node incarnation, the owner kind, and the
//! deployment. The node admits it only from the manager, for its own incarnation and the
//! session whose view it installed, within a fixed number of concurrent reads, and then asks the
//! deployment's owner to read its exact local runtime. The owner's protected launch values are
//! redacted from every entry before it is cut to 64 KiB, and a history drops its oldest entries
//! to stay within one response budget, saying so in its truncation flag. A follow of every
//! source stays open while the deployment's sources come and go, announcing each one that starts
//! or starts again, in order per source and without repeating delivered entries, until the
//! deployment is removed. Everything after the caller check is independent of the private
//! listener carrying the request.

use std::{
    collections::BTreeMap,
    num::NonZeroU16,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError, RwLock},
    time::{Duration, SystemTime},
};

use dotcore_proto::dotcore::deployment::private::v1::{
    DeploymentLogFollowOpened, DeploymentLogHistories, DeploymentLogHistory,
    DeploymentLogRejection, DeploymentLogRejectionReason, DeploymentLogResumePosition,
    DeploymentLogSource, DeploymentLogSourceEnded, DeploymentLogSourceEntry,
    DeploymentLogSourceHistory, DeploymentLogSourceOpening, DeploymentLogSourceUnavailable,
    DeploymentLogTarget, DeploymentOwnerKind, FollowDeploymentLogsRequest,
    FollowDeploymentLogsResponse, ReadDeploymentLogsRequest, ReadDeploymentLogsResponse,
    deployment_log_service_server::{
        DeploymentLogService as DeploymentLogServiceContract, DeploymentLogServiceServer,
    },
    deployment_log_source_history, follow_deployment_logs_response, read_deployment_logs_response,
};
use dotcore_runtime::{
    MAX_RUNTIME_LOG_SOURCES, RuntimeBackendError, RuntimeLogEntry, RuntimeLogEvent,
    RuntimeLogQuery, RuntimeLogSource, RuntimeLogSourceHistory, RuntimeLogSourceStream,
    RuntimeLogStream,
};
use dotcore_site::{
    private_transport::{AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext},
    view::fencing::{AcceptedNodeViewAuthority, ServingAuthorizationError},
};
use futures_util::StreamExt;
use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::{debug, error, warn};

use super::{
    DeploymentLogOwner, MESSAGE_LIMIT_BYTES, OwnerLogSources, RESPONSE_BUDGET_BYTES,
    WorkloadChange, WorkloadChanges, lifecycle::MissedWorkloadChanges, private_entry, system_time,
    unavailable_reason,
};
use crate::management_logs::{
    DeploymentLogDiagnosticContext, DeploymentLogError, DeploymentLogFollowQuery,
    DeploymentLogReadQuery, ManagementLogAdmission, ProtectedLogValues, record_source_failure,
};

/// Ordered frames of one follow as sent to the manager.
type FollowFrames =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, Status>> + Send>>;

/// Frames buffered toward the manager before the source readers wait for it.
const FRAME_BUFFER: usize = 64;

/// Longest a follow of a removed deployment waits for its open sources to end, so their last
/// lines still arrive while a source that never ends cannot hold the follow.
const REMOVAL_GRACE: Duration = Duration::from_secs(5);

/// Node-hosted private receiver for manager deployment-log reads.
pub(crate) struct DeploymentLogService {
    reader: DeploymentLogReader,
    authorization: AuthenticatedRoleAuthorization,
}

impl DeploymentLogService {
    /// Creates a receiver authorized for the manager that answers through `reader`.
    pub(crate) fn new(reader: DeploymentLogReader, context: &PrivateServiceContext) -> Self {
        Self {
            reader,
            authorization: context.authorize_role(PrivateNodeRole::Manager),
        }
    }

    /// Wraps the receiver in its gRPC server with limits sized for one full history.
    pub(crate) fn into_server(self) -> DeploymentLogServiceServer<Self> {
        DeploymentLogServiceServer::new(self)
            .max_decoding_message_size(MESSAGE_LIMIT_BYTES)
            .max_encoding_message_size(MESSAGE_LIMIT_BYTES)
    }

    /// Checks that this node serves deployment logs and that the caller is the manager.
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if self.authorization.receiver_role() != PrivateNodeRole::Worker {
            return Err(Status::permission_denied(
                "deployment logs are served only by nodes",
            ));
        }
        self.authorization.authorize(request)?;
        Ok(())
    }
}

#[tonic::async_trait]
impl DeploymentLogServiceContract for DeploymentLogService {
    type FollowDeploymentLogsStream = FollowFrames;

    async fn read_deployment_logs(
        &self,
        request: Request<ReadDeploymentLogsRequest>,
    ) -> Result<Response<ReadDeploymentLogsResponse>, Status> {
        self.authorize(&request)?;
        self.reader
            .read(request.into_inner())
            .await
            .map(Response::new)
    }

    async fn follow_deployment_logs(
        &self,
        request: Request<FollowDeploymentLogsRequest>,
    ) -> Result<Response<Self::FollowDeploymentLogsStream>, Status> {
        self.authorize(&request)?;
        self.reader
            .follow(request.into_inner())
            .await
            .map(Response::new)
    }
}

/// Node-local deployment-log reads for the deployments assigned in the node's installed view,
/// whatever transport carries the manager's request.
///
/// Each request is fenced by manager session and node incarnation, admitted within a fixed
/// number of concurrent reads, and answered with redacted entries within the response budget.
pub(crate) struct DeploymentLogReader {
    view: AcceptedNodeViewAuthority,
    owners: DeploymentLogOwners,
    admission: ManagementLogAdmission,

    /// Longest a follow of a removed deployment waits for its open sources to end.
    removal_grace: Duration,
}

impl DeploymentLogReader {
    /// Creates a reader dispatching each deployment to the owner of its kind.
    pub(crate) fn new(
        view: AcceptedNodeViewAuthority,
        owners: DeploymentLogOwners,
        admission: ManagementLogAdmission,
    ) -> Self {
        Self {
            view,
            owners,
            admission,
            removal_grace: REMOVAL_GRACE,
        }
    }

    /// Reads one bounded history of the requested deployment, or says why it was refused.
    pub(crate) async fn read(
        &self,
        request: ReadDeploymentLogsRequest,
    ) -> Result<ReadDeploymentLogsResponse, Status> {
        let outcome = match self.admit(request.target.as_ref()) {
            // The permit is held until the read is answered.
            Ok((admitted, _permit)) => self.read_admitted(&admitted, request).await?,
            Err(reason) => rejected_read_before_admission(request.target.as_ref(), reason),
        };
        Ok(ReadDeploymentLogsResponse {
            outcome: Some(outcome),
        })
    }

    /// Opens a follow of the requested deployment; a refusal is its single frame.
    pub(crate) async fn follow(
        &self,
        request: FollowDeploymentLogsRequest,
    ) -> Result<FollowFrames, Status> {
        match self.admit(request.target.as_ref()) {
            Ok((admitted, permit)) => self.follow_admitted(admitted, permit, request).await,
            Err(reason) => {
                record_rejection(request.target.as_ref(), "follow", reason);
                Ok(single_frame(
                    follow_deployment_logs_response::Frame::Rejected(rejection(reason)),
                ))
            }
        }
    }

    /// Checks the target identity and admission, then returns the owner to ask and the
    /// admission permit the request holds until it ends.
    fn admit(
        &self,
        target: Option<&DeploymentLogTarget>,
    ) -> Result<(AdmittedRequest, OwnedSemaphorePermit), DeploymentLogRejectionReason> {
        let target = target.ok_or(DeploymentLogRejectionReason::InvalidRequest)?;
        let (owner, owner_kind) = match DeploymentOwnerKind::try_from(target.owner_kind) {
            Ok(DeploymentOwnerKind::Model) => (Some(Arc::clone(&self.owners.model)), "model"),
            Ok(DeploymentOwnerKind::Application) => {
                (self.owners.application.clone(), "application")
            }
            _ => return Err(DeploymentLogRejectionReason::InvalidRequest),
        };
        if target.deployment_id.is_empty() {
            return Err(DeploymentLogRejectionReason::InvalidRequest);
        }
        self.view
            .authorize_manager_request(&target.manager_session_id, &target.node_incarnation_id)
            .map_err(|error| match error {
                ServingAuthorizationError::NodeIncarnationMismatch => {
                    DeploymentLogRejectionReason::NodeIncarnationMismatch
                }
                ServingAuthorizationError::ManagerSessionMismatch
                | ServingAuthorizationError::DeploymentNotAssigned => {
                    DeploymentLogRejectionReason::ManagerSessionMismatch
                }
            })?;
        let owner = owner.ok_or(DeploymentLogRejectionReason::DeploymentNotFound)?;
        let permit = self
            .admission
            .try_acquire()
            .map_err(|_| DeploymentLogRejectionReason::CapacityExhausted)?;
        let admitted = AdmittedRequest {
            owner,
            owner_kind,
            deployment_id: target.deployment_id.clone(),
        };
        Ok((admitted, permit))
    }

    async fn read_admitted(
        &self,
        admitted: &AdmittedRequest,
        request: ReadDeploymentLogsRequest,
    ) -> Result<read_deployment_logs_response::Outcome, Status> {
        let context = admitted.diagnostic_context("node_read_history");
        let query = match read_query(request) {
            Ok(query) => query,
            Err(reason) => return Ok(rejected_read(admitted, reason)),
        };
        match admitted
            .owner
            .read_logs(&admitted.deployment_id, query)
            .await
        {
            Ok(histories) => Ok(read_deployment_logs_response::Outcome::Histories(
                bounded_histories(histories, &context),
            )),
            Err(error) => {
                owner_refusal(admitted, error).map(|reason| rejected_read(admitted, reason))
            }
        }
    }

    async fn follow_admitted(
        &self,
        admitted: AdmittedRequest,
        permit: OwnedSemaphorePermit,
        request: FollowDeploymentLogsRequest,
    ) -> Result<FollowFrames, Status> {
        let cancellation = CancellationToken::new();
        let query = match follow_query(request, cancellation.clone()) {
            Ok(query) => query,
            Err(reason) => return Ok(rejected_follow(&admitted, reason)),
        };
        let resume_positions = query.resume_positions().clone();
        let OwnerLogSources {
            sources: streams,
            protected,
            late,
            changes,
        } = match admitted
            .owner
            .follow_logs(&admitted.deployment_id, query)
            .await
        {
            Ok(streams) => streams,
            Err(error) => {
                return owner_refusal(&admitted, error)
                    .map(|reason| rejected_follow(&admitted, reason));
            }
        };
        let context = admitted.diagnostic_context("node_follow");
        let (frames, receiver) = mpsc::channel(FRAME_BUFFER);
        let mut openings = Vec::with_capacity(streams.len());
        let mut followed = Vec::new();
        for RuntimeLogSourceStream { source, result } in streams {
            let unavailable = match result {
                Ok(stream) => {
                    followed.push((source.clone(), stream));
                    None
                }
                Err(error) => {
                    record_source_failure(&context, Some(&source), &error, &protected);
                    Some(unavailable_reason(&error) as i32)
                }
            };
            openings.push(DeploymentLogSourceOpening {
                source: Some(private_source(&source)),
                unavailable,
            });
        }
        frames
            .try_send(Ok(follow_frame(
                follow_deployment_logs_response::Frame::Opened(DeploymentLogFollowOpened {
                    sources: openings,
                }),
            )))
            .expect("the first frame fits the empty buffer");
        debug!(
            event_kind = "node_deployment_log_follow",
            owner_kind = admitted.owner_kind,
            deployment_id = %admitted.deployment_id,
            "Node opened a deployment-log follow for the manager"
        );
        let (ends, ended) = mpsc::channel(1);
        let mut forwarders = SourceForwarders::new(FollowShared {
            frames,
            admitted: Arc::new(admitted),
            protected: Arc::new(RwLock::new(protected)),
            delivered: Arc::new(Mutex::new(resume_positions)),
            context,
            closing: cancellation.child_token(),
            cancellation: cancellation.clone(),
            ends,
        });
        for (source, stream) in followed {
            forwarders.forward_opened(source, stream);
        }
        if late.is_some() || changes.is_some() {
            tokio::spawn(announce_later_sources(
                LaterSources {
                    late,
                    changes,
                    ended,
                },
                forwarders,
                self.removal_grace,
            ));
        }
        let stream = tokio_stream::wrappers::ReceiverStream::new(receiver).map(move |frame| {
            // The admission permit lives as long as the manager reads the follow.
            let _permit = &permit;
            frame
        });
        Ok(Box::pin(crate::management_logs::CancellableLogStream::new(
            stream,
            cancellation,
        )))
    }

    /// Waits at most `grace` for the open sources of a removed deployment's follows to end.
    #[cfg(test)]
    pub(crate) fn with_removal_grace(mut self, grace: Duration) -> Self {
        self.removal_grace = grace;
        self
    }
}

/// The node's log owner per deployment owner kind.
#[derive(Clone)]
pub(crate) struct DeploymentLogOwners {
    /// Model deployments assigned in the installed view.
    pub(crate) model: Arc<dyn DeploymentLogOwner>,

    /// Application deployments assigned in the installed view; absent on a node without
    /// application targets, which can be assigned none.
    pub(crate) application: Option<Arc<dyn DeploymentLogOwner>>,
}

/// One admitted request: the deployment and the owner to ask about it.
struct AdmittedRequest {
    owner: Arc<dyn DeploymentLogOwner>,
    owner_kind: &'static str,
    deployment_id: String,
}

impl AdmittedRequest {
    fn diagnostic_context(&self, operation: &'static str) -> DeploymentLogDiagnosticContext {
        DeploymentLogDiagnosticContext::new(self.owner_kind, self.deployment_id.clone(), operation)
    }
}

/// What a follow of every source learns after it opened: the sources that start, such as a
/// preparation step, the changes of the deployment's workload, and why the follow must end.
struct LaterSources {
    late: Option<mpsc::Receiver<RuntimeLogSourceStream>>,
    changes: Option<WorkloadChanges>,
    ended: mpsc::Receiver<FollowEnd>,
}

/// Announces the sources that start, or start again, after the follow opened, until the follow
/// is cancelled, the deployment is removed, or nothing more can start. It never waits on a
/// source, so a slow manager makes it miss no change.
///
/// A removal ends the follow cleanly once its open sources end, within `removal_grace`; a
/// failure to follow the changed workload, or missed changes, end it with an error status
/// instead, so the manager never mistakes them for a removal.
async fn announce_later_sources(
    later: LaterSources,
    mut forwarders: SourceForwarders,
    removal_grace: Duration,
) {
    let LaterSources {
        mut late,
        mut changes,
        mut ended,
    } = later;
    let shared = forwarders.shared.clone();
    while late.is_some() || changes.is_some() {
        let end = tokio::select! {
            () = shared.cancellation.cancelled() => return,
            source_stream = next_late(&mut late) => {
                match source_stream {
                    Some(source_stream) => forwarders.announce(source_stream),
                    None => late = None,
                }
                continue;
            }
            change = next_change(&mut changes) => match change {
                Some(Ok(WorkloadChange::Started { source_name })) => {
                    forwarders.reopen(source_name);
                    continue;
                }
                Some(Ok(WorkloadChange::Removed)) => FollowEnd::Removed,
                Some(Err(missed)) => {
                    warn!(
                        event_kind = "node_deployment_log_follow_failed",
                        owner_kind = shared.admitted.owner_kind,
                        deployment_id = %shared.admitted.deployment_id,
                        error = %missed,
                        "Node deployment-log follow lost track of its workload"
                    );
                    FollowEnd::Failed(Status::unavailable(
                        "deployment log follow missed workload changes",
                    ))
                }
                None => {
                    changes = None;
                    continue;
                }
            },
            Some(end) = ended.recv() => end,
        };
        match end {
            FollowEnd::Removed => {
                debug!(
                    event_kind = "node_deployment_log_follow_ended",
                    owner_kind = shared.admitted.owner_kind,
                    deployment_id = %shared.admitted.deployment_id,
                    "Node ends a deployment-log follow of a removed deployment"
                );
                forwarders.drain(removal_grace).await;
            }
            FollowEnd::Failed(status) => {
                shared.send(Err(status)).await;
            }
        }
        // Stops every source so the follow ends once its frames drain.
        shared.cancellation.cancel();
        return;
    }
}

/// Why a follow of every source ends while its manager still reads it.
enum FollowEnd {
    /// The deployment is gone from the node.
    Removed,

    /// The follow can no longer tell which workload to follow.
    Failed(Status),
}

async fn next_late(
    late: &mut Option<mpsc::Receiver<RuntimeLogSourceStream>>,
) -> Option<RuntimeLogSourceStream> {
    match late {
        Some(late) => late.recv().await,
        None => std::future::pending().await,
    }
}

async fn next_change(
    changes: &mut Option<WorkloadChanges>,
) -> Option<Result<WorkloadChange, MissedWorkloadChanges>> {
    match changes {
        Some(changes) => changes.next().await,
        None => std::future::pending().await,
    }
}

/// The source forwarders of one follow, one per source name, so frames for a name stay in
/// order whichever stream of that source they come from.
struct SourceForwarders {
    shared: FollowShared,

    /// Per source name, where to ask its forwarder to open the source again.
    reopens: BTreeMap<String, mpsc::Sender<()>>,

    /// Held by every forwarder, so the follow knows when all of them stopped.
    running: mpsc::Sender<()>,
    stopped: mpsc::Receiver<()>,
}

impl SourceForwarders {
    fn new(shared: FollowShared) -> Self {
        let (running, stopped) = mpsc::channel(1);
        Self {
            shared,
            reopens: BTreeMap::new(),
            running,
            stopped,
        }
    }

    /// Forwards a source the opening frame listed as open.
    fn forward_opened(&mut self, source: RuntimeLogSource, stream: RuntimeLogStream) {
        self.spawn(
            source.name.clone(),
            ForwarderStart::Opened(ForwardedStream {
                source,
                stream,
                after: None,
            }),
        );
    }

    /// Announces a source that started after the follow opened, then forwards it. Such a
    /// source starts once, so one already forwarded is left as it is.
    fn announce(&mut self, source_stream: RuntimeLogSourceStream) {
        if !self.reopens.contains_key(&source_stream.source.name) {
            self.spawn(
                source_stream.source.name.clone(),
                ForwarderStart::Announced(source_stream),
            );
        }
    }

    /// Asks the forwarder of a workload source to open it again once its current stream, if
    /// any, ended. Requests made before it opens it again count as one.
    fn reopen(&mut self, source_name: String) {
        match self.reopens.get(&source_name) {
            // A full channel already holds a request; a closed one belongs to an ending follow.
            Some(reopen) => {
                let _ = reopen.try_send(());
            }
            None => self.spawn(source_name, ForwarderStart::Reopen),
        }
    }

    /// Lets every forwarder finish its current stream without opening another, waiting at
    /// most `grace` for them all to stop.
    async fn drain(self, grace: Duration) {
        let Self {
            shared,
            reopens,
            running,
            mut stopped,
        } = self;
        shared.closing.cancel();
        drop((reopens, running));
        tokio::select! {
            () = shared.cancellation.cancelled() => {}
            _ = tokio::time::timeout(grace, stopped.recv()) => {}
        }
    }

    fn spawn(&mut self, source_name: String, start: ForwarderStart) {
        let (reopen, reopens) = mpsc::channel(1);
        tokio::spawn(forward_source(
            self.shared.clone(),
            source_name.clone(),
            start,
            reopens,
            self.running.clone(),
        ));
        self.reopens.insert(source_name, reopen);
    }
}

/// What every source forwarder of one follow shares.
#[derive(Clone)]
struct FollowShared {
    frames: mpsc::Sender<Result<FollowDeploymentLogsResponse, Status>>,

    /// The deployment followed and the owner that opens its sources again.
    admitted: Arc<AdmittedRequest>,

    /// Values redacted from every entry and diagnostic of the follow; they only grow, as each
    /// workload announcement adds those of its launch.
    protected: Arc<RwLock<ProtectedLogValues>>,

    /// Latest entry timestamp delivered per source, starting from the request's resume
    /// positions.
    delivered: Arc<Mutex<BTreeMap<String, SystemTime>>>,
    context: DeploymentLogDiagnosticContext,

    /// Fires once no source may open anymore, as when the deployment was removed.
    closing: CancellationToken,
    cancellation: CancellationToken,

    /// Reports why the follow must end.
    ends: mpsc::Sender<FollowEnd>,
}

impl FollowShared {
    /// Sends one frame unless the follow was cancelled; `false` once the manager is gone.
    async fn send(&self, frame: Result<FollowDeploymentLogsResponse, Status>) -> bool {
        tokio::select! {
            () = self.cancellation.cancelled() => false,
            sent = self.frames.send(frame) => sent.is_ok(),
        }
    }

    fn end(&self, end: FollowEnd) {
        // A follow already ending needs no second reason.
        let _ = self.ends.try_send(end);
    }

    fn protect(&self, values: ProtectedLogValues) {
        self.protected
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(values);
    }

    fn delivered(&self, source_name: &str) -> Option<SystemTime> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(source_name)
            .copied()
    }

    fn record_delivered(&self, source_name: &str, timestamp: SystemTime) {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(source_name.to_owned())
            .and_modify(|latest| *latest = (*latest).max(timestamp))
            .or_insert(timestamp);
    }

    /// Records a source failure without its protected values and names its typed code.
    fn unavailable(&self, source: &RuntimeLogSource, error: &RuntimeBackendError) -> i32 {
        let protected = self
            .protected
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        record_source_failure(&self.context, Some(source), error, &protected);
        unavailable_reason(error) as i32
    }

    fn entry_frame(
        &self,
        source: &RuntimeLogSource,
        entry: RuntimeLogEntry,
    ) -> follow_deployment_logs_response::Frame {
        let protected = self
            .protected
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        follow_deployment_logs_response::Frame::Entry(DeploymentLogSourceEntry {
            source_name: source.name.clone(),
            entry: Some(private_entry(entry, &protected)),
        })
    }
}

/// How a source forwarder begins.
enum ForwarderStart {
    /// With a stream the opening frame listed.
    Opened(ForwardedStream),

    /// With a stream to announce first.
    Announced(RuntimeLogSourceStream),

    /// By opening the source again through its owner.
    Reopen,
}

/// One stream of a source being forwarded.
struct ForwardedStream {
    source: RuntimeLogSource,
    stream: RuntimeLogStream,

    /// Entries at or before this time were already delivered or precede the resume position,
    /// so they are skipped, as when a workload restarted in place serves its earlier output.
    after: Option<SystemTime>,
}

/// The manager went away, so nothing more can be forwarded.
struct FollowClosed;

/// Forwards every stream of one source name in turn. A request to open the source again while
/// a stream runs waits for that stream's terminal frame, then the owner resolves the source as
/// it is by then, after the last entry delivered. Stops when the follow is cancelled or the
/// manager goes away, or once its stream is done and no source may open anymore.
async fn forward_source(
    shared: FollowShared,
    source_name: String,
    start: ForwarderStart,
    mut reopens: mpsc::Receiver<()>,
    _running: mpsc::Sender<()>,
) {
    let (mut current, mut wanted) = match start {
        ForwarderStart::Opened(forwarded) => (Some(forwarded), false),
        ForwarderStart::Announced(source_stream) => {
            match open_announced(&shared, source_stream).await {
                Ok(opened) => (opened, false),
                Err(FollowClosed) => return,
            }
        }
        ForwarderStart::Reopen => (None, true),
    };
    let mut accepting = true;
    loop {
        let Some(forwarded) = current.as_mut() else {
            if !wanted && accepting {
                wanted = tokio::select! {
                    () = shared.closing.cancelled() => return,
                    reopen = reopens.recv() => reopen.is_some(),
                };
                accepting = wanted;
            }
            if !wanted || shared.closing.is_cancelled() {
                return;
            }
            wanted = false;
            match reopen_source(&shared, &source_name).await {
                Ok(opened) => current = opened,
                Err(FollowClosed) => return,
            }
            continue;
        };
        tokio::select! {
            () = shared.cancellation.cancelled() => return,
            reopen = reopens.recv(), if accepting => match reopen {
                Some(()) => wanted = true,
                None => accepting = false,
            },
            event = forwarded.stream.next() => {
                match forward_event(&shared, &forwarded.source, forwarded.after, event).await {
                    Ok(false) => {}
                    Ok(true) => current = None,
                    Err(FollowClosed) => return,
                }
            }
        }
    }
}

/// Opens one workload source again as its owner resolves it now, after the last entry this
/// follow delivered for it, or else after the request's resume position for it, adding the
/// values protected for its launch to the follow's; returns its stream to forward, or `None`
/// when it opened as unavailable or the follow must end.
async fn reopen_source(
    shared: &FollowShared,
    source_name: &str,
) -> Result<Option<ForwardedStream>, FollowClosed> {
    let admitted = &shared.admitted;
    let query = DeploymentLogFollowQuery::new(
        vec![source_name.to_owned()],
        shared
            .delivered(source_name)
            .map(|position| (source_name.to_owned(), position))
            .into_iter()
            .collect(),
        shared.cancellation.clone(),
    );
    let reopened = tokio::select! {
        () = shared.cancellation.cancelled() => return Err(FollowClosed),
        reopened = admitted.owner.follow_logs(&admitted.deployment_id, query) => reopened,
    };
    let reopened = match reopened {
        Ok(reopened) => reopened,
        Err(error) => {
            shared.end(match owner_refusal(admitted, error) {
                Ok(DeploymentLogRejectionReason::DeploymentNotFound) => FollowEnd::Removed,
                Ok(reason) => {
                    record_admitted_rejection(admitted, "follow", reason);
                    FollowEnd::Failed(Status::internal("deployment log resolution failed"))
                }
                Err(status) => FollowEnd::Failed(status),
            });
            return Ok(None);
        }
    };
    debug!(
        event_kind = "node_deployment_log_source_reopened",
        owner_kind = admitted.owner_kind,
        deployment_id = %admitted.deployment_id,
        source_name,
        "Node deployment-log follow opens a changed workload source again"
    );
    shared.protect(reopened.protected);
    let Some(source_stream) = reopened
        .sources
        .into_iter()
        .find(|source_stream| source_stream.source.name == source_name)
    else {
        return Ok(None);
    };
    open_announced(shared, source_stream).await
}

/// Sends the opening frame of an announced source; returns its stream to forward, or `None`
/// when it opened as unavailable.
async fn open_announced(
    shared: &FollowShared,
    source_stream: RuntimeLogSourceStream,
) -> Result<Option<ForwardedStream>, FollowClosed> {
    let RuntimeLogSourceStream { source, result } = source_stream;
    let unavailable = result
        .as_ref()
        .err()
        .map(|error| shared.unavailable(&source, error));
    let opened = follow_frame(follow_deployment_logs_response::Frame::SourceOpened(
        DeploymentLogSourceOpening {
            source: Some(private_source(&source)),
            unavailable,
        },
    ));
    if !shared.send(Ok(opened)).await {
        return Err(FollowClosed);
    }
    Ok(result.ok().map(|stream| ForwardedStream {
        after: shared.delivered(&source.name),
        source,
        stream,
    }))
}

/// Sends the frame of one event of a stream skipping entries at or before `after`; returns
/// whether it was the stream's terminal frame.
async fn forward_event(
    shared: &FollowShared,
    source: &RuntimeLogSource,
    after: Option<SystemTime>,
    event: Option<Result<RuntimeLogEvent, RuntimeBackendError>>,
) -> Result<bool, FollowClosed> {
    let (frame, delivered) = match event {
        Some(Ok(RuntimeLogEvent::Entry(entry))) => {
            if after.is_some_and(|after| entry.timestamp <= after) {
                return Ok(false);
            }
            let timestamp = entry.timestamp;
            (shared.entry_frame(source, entry), Some(timestamp))
        }
        Some(Ok(RuntimeLogEvent::SourceUnavailable(_))) => (
            follow_deployment_logs_response::Frame::SourceUnavailable(
                DeploymentLogSourceUnavailable {
                    source_name: source.name.clone(),
                    reason: None,
                },
            ),
            None,
        ),
        Some(Err(error)) => (
            follow_deployment_logs_response::Frame::SourceUnavailable(
                DeploymentLogSourceUnavailable {
                    source_name: source.name.clone(),
                    reason: Some(shared.unavailable(source, &error)),
                },
            ),
            None,
        ),
        None => (
            follow_deployment_logs_response::Frame::SourceEnded(DeploymentLogSourceEnded {
                source_name: source.name.clone(),
            }),
            None,
        ),
    };
    if !shared.send(Ok(follow_frame(frame))).await {
        return Err(FollowClosed);
    }
    // Every frame but an entry ends the stream.
    match delivered {
        Some(timestamp) => {
            shared.record_delivered(&source.name, timestamp);
            Ok(false)
        }
        None => Ok(true),
    }
}

/// Converts owner histories to their private form within the response budget.
///
/// Protected values are redacted and entries cut to the entry limit first; then the oldest entry
/// of any source is dropped until the encoded response fits, and each source that lost entries
/// says so.
fn bounded_histories(
    histories: OwnerLogSources<RuntimeLogSourceHistory>,
    context: &DeploymentLogDiagnosticContext,
) -> DeploymentLogHistories {
    let OwnerLogSources {
        sources: histories,
        protected,
        ..
    } = histories;
    let sources = histories
        .into_iter()
        .map(|RuntimeLogSourceHistory { source, result }| {
            let result = match result {
                Ok(history) => {
                    deployment_log_source_history::Result::History(DeploymentLogHistory {
                        entries: history
                            .entries
                            .into_iter()
                            .map(|entry| private_entry(entry, &protected))
                            .collect(),
                        has_older_output: history.has_older_output,
                        candidate_budget_truncated: history.candidate_budget_truncated,
                    })
                }
                Err(error) => {
                    record_source_failure(context, Some(&source), &error, &protected);
                    deployment_log_source_history::Result::Unavailable(
                        unavailable_reason(&error) as i32
                    )
                }
            };
            DeploymentLogSourceHistory {
                source: Some(private_source(&source)),
                result: Some(result),
            }
        })
        .collect();
    let mut histories = DeploymentLogHistories { sources };
    // Removing an entry shrinks the enclosing length prefixes too, so the running size is an
    // upper bound of the encoded response.
    let mut size = ReadDeploymentLogsResponse {
        outcome: Some(read_deployment_logs_response::Outcome::Histories(
            histories.clone(),
        )),
    }
    .encoded_len();
    while size > RESPONSE_BUDGET_BYTES {
        let Some(oldest) = histories
            .sources
            .iter_mut()
            .filter_map(|source| match source.result.as_mut() {
                Some(deployment_log_source_history::Result::History(history))
                    if !history.entries.is_empty() =>
                {
                    Some(history)
                }
                _ => None,
            })
            .min_by_key(|history| {
                history.entries[0]
                    .timestamp
                    .map(|timestamp| (timestamp.seconds, timestamp.nanos))
            })
        else {
            break;
        };
        let removed = oldest.entries.remove(0).encoded_len();
        oldest.candidate_budget_truncated = true;
        size -= 1 + prost::length_delimiter_len(removed) + removed;
    }
    histories
}

fn read_query(
    request: ReadDeploymentLogsRequest,
) -> Result<DeploymentLogReadQuery, DeploymentLogRejectionReason> {
    validate_sources(&request.source_names)?;
    let maximum_entries = u16::try_from(request.maximum_entries)
        .ok()
        .and_then(NonZeroU16::new)
        .ok_or(DeploymentLogRejectionReason::InvalidRequest)?;
    let before = request
        .before_timestamp
        .map(|timestamp| system_time(timestamp).ok_or(DeploymentLogRejectionReason::InvalidRequest))
        .transpose()?;
    let history = RuntimeLogQuery::new(maximum_entries, before)
        .map_err(|_| DeploymentLogRejectionReason::InvalidRequest)?;
    Ok(DeploymentLogReadQuery::new(request.source_names, history))
}

fn follow_query(
    request: FollowDeploymentLogsRequest,
    cancellation: CancellationToken,
) -> Result<DeploymentLogFollowQuery, DeploymentLogRejectionReason> {
    validate_sources(&request.source_names)?;
    let mut positions: BTreeMap<String, SystemTime> = BTreeMap::new();
    for DeploymentLogResumePosition {
        source_name,
        latest_timestamp,
    } in request.resume_positions
    {
        let selected =
            request.source_names.is_empty() || request.source_names.contains(&source_name);
        let timestamp = latest_timestamp
            .and_then(system_time)
            .filter(|_| selected && !positions.contains_key(&source_name))
            .ok_or(DeploymentLogRejectionReason::InvalidRequest)?;
        positions.insert(source_name, timestamp);
    }
    Ok(DeploymentLogFollowQuery::new(
        request.source_names,
        positions,
        cancellation,
    ))
}

fn validate_sources(source_names: &[String]) -> Result<(), DeploymentLogRejectionReason> {
    let unique = source_names
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    if source_names.len() > MAX_RUNTIME_LOG_SOURCES
        || unique != source_names.len()
        || source_names.iter().any(String::is_empty)
    {
        return Err(DeploymentLogRejectionReason::InvalidRequest);
    }
    Ok(())
}

/// Maps an owner refusal to a rejection; an owner resolution failure stays an internal error.
fn owner_refusal(
    admitted: &AdmittedRequest,
    error: DeploymentLogError,
) -> Result<DeploymentLogRejectionReason, Status> {
    match error {
        DeploymentLogError::NotFound | DeploymentLogError::UnsupportedDeployment => {
            Ok(DeploymentLogRejectionReason::DeploymentNotFound)
        }
        DeploymentLogError::InvalidSource => Ok(DeploymentLogRejectionReason::InvalidSource),
        // Owners read their own node's runtime, so this only stands for a node they rely on.
        DeploymentLogError::NodeUnavailable => {
            Err(Status::unavailable("deployment logs are unavailable"))
        }
        DeploymentLogError::Configuration(source) => {
            error!(
                event_kind = "node_deployment_log_failed",
                owner_kind = admitted.owner_kind,
                deployment_id = %admitted.deployment_id,
                error = format!("{source:#}"),
                "Node could not resolve deployment logs for the manager"
            );
            Err(Status::internal("deployment log resolution failed"))
        }
    }
}

fn rejected_read(
    admitted: &AdmittedRequest,
    reason: DeploymentLogRejectionReason,
) -> read_deployment_logs_response::Outcome {
    record_admitted_rejection(admitted, "read_history", reason);
    read_deployment_logs_response::Outcome::Rejected(rejection(reason))
}

fn rejected_read_before_admission(
    target: Option<&DeploymentLogTarget>,
    reason: DeploymentLogRejectionReason,
) -> read_deployment_logs_response::Outcome {
    record_rejection(target, "read_history", reason);
    read_deployment_logs_response::Outcome::Rejected(rejection(reason))
}

fn rejected_follow(
    admitted: &AdmittedRequest,
    reason: DeploymentLogRejectionReason,
) -> FollowFrames {
    record_admitted_rejection(admitted, "follow", reason);
    single_frame(follow_deployment_logs_response::Frame::Rejected(rejection(
        reason,
    )))
}

fn record_admitted_rejection(
    admitted: &AdmittedRequest,
    operation: &'static str,
    reason: DeploymentLogRejectionReason,
) {
    debug!(
        event_kind = "node_deployment_log_rejected",
        operation,
        owner_kind = admitted.owner_kind,
        deployment_id = %admitted.deployment_id,
        reason = reason.as_str_name(),
        "Node rejected a manager deployment-log request"
    );
}

fn record_rejection(
    target: Option<&DeploymentLogTarget>,
    operation: &'static str,
    reason: DeploymentLogRejectionReason,
) {
    debug!(
        event_kind = "node_deployment_log_rejected",
        operation,
        deployment_id = target.map_or("", |target| target.deployment_id.as_str()),
        reason = reason.as_str_name(),
        "Node rejected a manager deployment-log request"
    );
}

fn rejection(reason: DeploymentLogRejectionReason) -> DeploymentLogRejection {
    DeploymentLogRejection {
        reason: reason as i32,
    }
}

fn single_frame(frame: follow_deployment_logs_response::Frame) -> FollowFrames {
    Box::pin(tokio_stream::once(Ok(follow_frame(frame))))
}

fn follow_frame(frame: follow_deployment_logs_response::Frame) -> FollowDeploymentLogsResponse {
    FollowDeploymentLogsResponse { frame: Some(frame) }
}

fn private_source(source: &RuntimeLogSource) -> DeploymentLogSource {
    DeploymentLogSource {
        name: source.name.clone(),
        label: source.label.clone(),
    }
}
