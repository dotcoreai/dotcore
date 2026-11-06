//! Reads the runtime logs of a node-hosted deployment for the site manager and hands them back as
//! ordinary per-source runtime results. A node in another process is read over the shared pinned
//! channel to it; the node sharing the manager process is read in process through the same
//! admission, bounds, and redaction its private service applies.
//!
//! For a history read, an unreachable, stale, busy, or slow node makes every selected source
//! unavailable instead of failing the request, and a deployment the node has not installed reads
//! as not created yet; only an unknown source is an error. A node truncation stays visible as
//! truncated history. A follow instead fails whenever the node cannot open or keep serving it, so
//! its caller can tell a failure from a follow that ended; dropping it stops the node's read.

use std::{collections::BTreeMap, pin::Pin, sync::Arc, time::Duration};

use dotcore_proto::dotcore::{
    application::v1::ApplicationDeploymentDesiredState,
    deployment::private::v1::{
        DeploymentLogHistories, DeploymentLogRejectionReason, DeploymentLogResumePosition,
        DeploymentLogSourceEntry, DeploymentLogSourceOpening, DeploymentLogTarget,
        DeploymentOwnerKind, FollowDeploymentLogsRequest, FollowDeploymentLogsResponse,
        ReadDeploymentLogsRequest, ReadDeploymentLogsResponse, deployment_log_service_client,
        deployment_log_source_history, follow_deployment_logs_response,
        read_deployment_logs_response,
    },
    model::v1::ModelDeploymentDesiredState,
    site::v1::Site,
};
use dotcore_runtime::{
    RuntimeBackendError, RuntimeLogEvent, RuntimeLogHistory, RuntimeLogSource,
    RuntimeLogSourceHistory, RuntimeLogSourceStream, RuntimeLogStream,
};
use dotcore_site::{
    node::control::{CoordinatorConnectionState, ManagerNodeControlCoordinator},
    repository::SiteRepository,
    view::fencing::{ManagerSession, NodeIncarnation},
    worker_channels::{WorkerChannels, WorkerConnection},
};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::{Status, transport::Channel};
use tracing::{debug, warn};

use super::{
    DeploymentLogReader, MESSAGE_LIMIT_BYTES, WORKER_LOG_DEADLINE, runtime_entry, runtime_error,
    timestamp,
};
use crate::management_logs::{
    DeploymentLogError, DeploymentLogFollowFailure, DeploymentLogFollowQuery,
    DeploymentLogReadQuery, DeploymentLogStreams,
};

/// Events buffered per followed source before the node stream waits for the reader.
const SOURCE_EVENT_BUFFER: usize = 64;

/// Sending half of one followed source's runtime event stream.
type SourceSender = mpsc::Sender<Result<RuntimeLogEvent, RuntimeBackendError>>;

/// Tells whether the manager still wants a followed deployment running, so a node that stopped
/// hosting it, as after a move, is told apart from a removal.
pub(super) type StillPresent = Arc<dyn Fn() -> bool + Send + Sync>;

/// The manager's reader of deployment logs on their hosting nodes.
///
/// The transport is chosen by node identity before any channel lookup: the node sharing the
/// manager process is read in process and is never a channel dial target.
#[derive(Clone)]
pub(crate) struct NodeDeploymentLogs {
    /// Channels to other nodes; absent on a manager without a private listener.
    channels: Option<WorkerChannels>,
    site: SiteRepository,
    coordinator: ManagerNodeControlCoordinator,
    local: Option<Arc<LocalNodeLogs>>,
}

/// Deployment-log reads of the node sharing the manager process.
struct LocalNodeLogs {
    node_id: String,
    reader: Arc<DeploymentLogReader>,
}

impl NodeDeploymentLogs {
    pub(crate) fn new(
        channels: Option<WorkerChannels>,
        site: SiteRepository,
        coordinator: ManagerNodeControlCoordinator,
    ) -> Self {
        Self {
            channels,
            site,
            coordinator,
            local: None,
        }
    }

    /// Reads deployments hosted on `node_id`, the SiteNode sharing the manager process, through
    /// its own log `reader`.
    pub(crate) fn with_local_node(mut self, node_id: String, reader: DeploymentLogReader) -> Self {
        self.local = Some(Arc::new(LocalNodeLogs {
            node_id,
            reader: Arc::new(reader),
        }));
        self
    }

    /// Reads one bounded history per selected source of a deployment hosted on `node_id`.
    ///
    /// `sources` is the owner's own selection; each comes back read or unavailable.
    pub(crate) async fn read(
        &self,
        request: NodeLogRequest<'_>,
        query: &DeploymentLogReadQuery,
    ) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
        match self.connect(&request) {
            Some(connection) => read_from(connection, request, query, WORKER_LOG_DEADLINE).await,
            None => Ok(unavailable_histories(request.fallback_sources())),
        }
    }

    /// Follows the selected sources of a deployment hosted on `node_id` until the query's
    /// cancellation fires, which also ends the node call. A follow selecting every source also
    /// receives the known sources the node starts or starts again later, and ends when the node
    /// removes the deployment. A node that cannot open the follow fails it.
    pub(crate) async fn follow(
        &self,
        request: NodeLogRequest<'_>,
        query: &DeploymentLogFollowQuery,
    ) -> Result<DeploymentLogStreams, DeploymentLogError> {
        match self.connect(&request) {
            Some(connection) => {
                let present = self.still_present(&request);
                follow_from(connection, request, query, present, WORKER_LOG_DEADLINE).await
            }
            None => Err(DeploymentLogError::NodeUnavailable),
        }
    }

    /// Checks whether the manager's own site still wants the requested deployment running. A
    /// site that cannot be read counts as wanting it, so the follow is reopened rather than
    /// ended.
    fn still_present(&self, request: &NodeLogRequest<'_>) -> StillPresent {
        let site = self.site.clone();
        let owner_kind = request.owner_kind;
        let deployment_id = request.deployment_id.to_owned();
        Arc::new(move || {
            site.with_site(|site| desired_present(site, owner_kind, &deployment_id))
                .unwrap_or(true)
        })
    }

    /// Returns the connection to the hosting node, or records why it is unreachable.
    fn connect(&self, request: &NodeLogRequest<'_>) -> Option<NodeLogConnection> {
        if let Some(local) = self
            .local
            .as_ref()
            .filter(|local| local.node_id == request.node_id)
        {
            let endpoint = self
                .coordinator
                .node_endpoint(request.node_id)
                .filter(|endpoint| {
                    endpoint.connection_state != CoordinatorConnectionState::Disconnected
                });
            let Some(endpoint) = endpoint else {
                request.record_unavailable("connect", "not_connected");
                return None;
            };
            return Some(NodeLogConnection {
                transport: NodeLogTransport::InProcess(Arc::clone(&local.reader)),
                manager_session: self.coordinator.manager_session().clone(),
                node_incarnation: endpoint.incarnation,
            });
        }
        let Some(channels) = &self.channels else {
            request.record_unavailable("connect", "private_transport_unconfigured");
            return None;
        };
        channels
            .connect(&self.site, &self.coordinator, request.node_id)
            .map(NodeLogConnection::from)
            .inspect_err(|error| {
                debug!(
                    event_kind = "node_deployment_log_unavailable",
                    node_id = request.node_id,
                    deployment_id = request.deployment_id,
                    reason = "channel_unavailable",
                    error = %error,
                    "Manager could not reach the node hosting a deployment for its logs"
                );
            })
            .ok()
    }
}

/// One reachable hosting node with the identities a log request to it must name.
pub(super) struct NodeLogConnection {
    transport: NodeLogTransport,
    manager_session: ManagerSession,
    node_incarnation: NodeIncarnation,
}

/// How one hosting node's log service is reached.
enum NodeLogTransport {
    /// Another node, over its pinned private diagnostics channel.
    Remote(Channel),

    /// The node sharing the manager process, through its own log reader.
    InProcess(Arc<DeploymentLogReader>),
}

/// Ordered follow frames from a hosting node, whatever transport carries them.
type NodeFollowFrames =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<FollowDeploymentLogsResponse, Status>> + Send>>;

impl From<WorkerConnection> for NodeLogConnection {
    fn from(connection: WorkerConnection) -> Self {
        Self {
            transport: NodeLogTransport::Remote(connection.channel),
            manager_session: connection.manager_session,
            node_incarnation: connection.node_incarnation,
        }
    }
}

impl NodeLogConnection {
    async fn read(
        self,
        message: ReadDeploymentLogsRequest,
    ) -> Result<ReadDeploymentLogsResponse, Status> {
        match self.transport {
            NodeLogTransport::Remote(channel) => client(channel)
                .read_deployment_logs(message)
                .await
                .map(tonic::Response::into_inner),
            NodeLogTransport::InProcess(reader) => reader.read(message).await,
        }
    }

    async fn follow(
        self,
        message: FollowDeploymentLogsRequest,
    ) -> Result<NodeFollowFrames, Status> {
        match self.transport {
            NodeLogTransport::Remote(channel) => {
                let frames = client(channel)
                    .follow_deployment_logs(message)
                    .await?
                    .into_inner();
                Ok(Box::pin(frames))
            }
            NodeLogTransport::InProcess(reader) => reader.follow(message).await,
        }
    }
}

/// One owner's log request for a deployment its own current declarations place on a node.
pub(crate) struct NodeLogRequest<'a> {
    pub(crate) node_id: &'a str,
    pub(crate) owner_kind: DeploymentOwnerKind,
    pub(crate) deployment_id: &'a str,
    pub(crate) selection: NodeLogSelection<'a>,
}

/// The sources one owner request selects on the hosting node, with the labels the owner issues.
#[derive(Clone, Copy)]
pub(crate) enum NodeLogSelection<'a> {
    /// Exactly these sources.
    Named(&'a [RuntimeLogSource]),

    /// Every source the node holds for the deployment among `known`, in the node's order, such
    /// as the preparation steps it ran; `defaults` stand for them when the node cannot answer.
    All {
        known: &'a [RuntimeLogSource],
        defaults: &'a [RuntimeLogSource],
    },
}

impl NodeLogRequest<'_> {
    fn target(&self, connection: &NodeLogConnection) -> DeploymentLogTarget {
        DeploymentLogTarget {
            manager_session_id: connection.manager_session.as_str().to_owned(),
            node_incarnation_id: connection.node_incarnation.as_str().to_owned(),
            owner_kind: self.owner_kind as i32,
            deployment_id: self.deployment_id.to_owned(),
        }
    }

    /// Names the selected sources; none lets the node answer with every source it holds.
    fn source_names(&self) -> Vec<String> {
        match self.selection {
            NodeLogSelection::Named(sources) => {
                sources.iter().map(|source| source.name.clone()).collect()
            }
            NodeLogSelection::All { .. } => Vec::new(),
        }
    }

    /// The sources reported when the node gives no per-source answer.
    fn fallback_sources(&self) -> &[RuntimeLogSource] {
        match self.selection {
            NodeLogSelection::Named(sources) => sources,
            NodeLogSelection::All { defaults, .. } => defaults,
        }
    }

    /// The sources the request reports for a node answer naming `answered`: the named sources,
    /// or every known source the node answered for, once each and with the owner's label.
    fn answered_sources<'a>(
        &self,
        answered: impl Iterator<Item = &'a str>,
    ) -> Vec<RuntimeLogSource> {
        match self.selection {
            NodeLogSelection::Named(sources) => sources.to_vec(),
            NodeLogSelection::All { known, .. } => {
                let mut sources = Vec::<RuntimeLogSource>::new();
                for name in answered {
                    if let Some(source) = known.iter().find(|source| source.name == name)
                        && !sources.contains(source)
                    {
                        sources.push(source.clone());
                    }
                }
                sources
            }
        }
    }

    /// Returns the failure every selected source takes for a rejection, or the request error it
    /// stands for.
    ///
    /// The caller's own declarations place the deployment on this node, so a node that has
    /// not installed it, or already withdrew it, has no workload for it: the same outcome a
    /// local deployment has before its workload is created.
    fn rejected(
        &self,
        operation: &'static str,
        reason: i32,
    ) -> Result<fn() -> RuntimeBackendError, DeploymentLogError> {
        let reason = DeploymentLogRejectionReason::try_from(reason);
        let name = reason.map_or("unknown_rejection", |reason| reason.as_str_name());
        self.record_unavailable(operation, name);
        match reason {
            Ok(DeploymentLogRejectionReason::DeploymentNotFound) => {
                Ok(RuntimeBackendError::workload_not_created)
            }
            Ok(DeploymentLogRejectionReason::InvalidSource) => {
                Err(DeploymentLogError::InvalidSource)
            }
            _ => Ok(RuntimeBackendError::remote_log_unavailable),
        }
    }

    fn record_unavailable(&self, operation: &'static str, reason: &str) {
        debug!(
            event_kind = "node_deployment_log_unavailable",
            operation,
            node_id = self.node_id,
            deployment_id = self.deployment_id,
            reason,
            "Node deployment logs are unavailable"
        );
    }
}

/// Reads one history per selected source from an already connected node, giving up after
/// `deadline`.
pub(super) async fn read_from(
    connection: impl Into<NodeLogConnection>,
    request: NodeLogRequest<'_>,
    query: &DeploymentLogReadQuery,
    deadline: Duration,
) -> Result<Vec<RuntimeLogSourceHistory>, DeploymentLogError> {
    let connection = connection.into();
    let message = ReadDeploymentLogsRequest {
        target: Some(request.target(&connection)),
        source_names: request.source_names(),
        maximum_entries: u32::from(query.history().maximum_entries().get()),
        before_timestamp: query.history().before_timestamp().map(timestamp),
    };
    #[expect(
        clippy::disallowed_methods,
        reason = "bounds a real call to a node's log service"
    )]
    let call = tokio::time::timeout(deadline, connection.read(message)).await;
    let outcome = match call {
        Ok(Ok(response)) => response.outcome,
        Ok(Err(status)) => {
            request.record_unavailable("read_history", status_reason(&status));
            return Ok(unavailable_histories(request.fallback_sources()));
        }
        Err(_) => {
            request.record_unavailable("read_history", "deadline_exceeded");
            return Ok(unavailable_histories(request.fallback_sources()));
        }
    };
    match outcome {
        Some(read_deployment_logs_response::Outcome::Histories(histories)) => {
            let sources = request.answered_sources(
                histories
                    .sources
                    .iter()
                    .filter_map(|history| history.source.as_ref())
                    .map(|source| source.name.as_str()),
            );
            Ok(adapt_histories(histories, &sources))
        }
        Some(read_deployment_logs_response::Outcome::Rejected(rejected)) => {
            let failure = request.rejected("read_history", rejected.reason)?;
            Ok(request
                .fallback_sources()
                .iter()
                .map(|source| RuntimeLogSourceHistory {
                    source: source.clone(),
                    result: Err(failure()),
                })
                .collect())
        }
        None => {
            request.record_unavailable("read_history", "protocol_violation");
            Ok(unavailable_histories(request.fallback_sources()))
        }
    }
}

/// Opens a follow of the selected sources on an already connected node, giving up on its opening
/// frame after `deadline`. A node that cannot open the follow, for any reason but an unknown
/// source, makes it fail as unavailable. When the node ends a follow of every source, the
/// follow ends cleanly only if `present` says the manager no longer wants the deployment running.
#[expect(
    clippy::disallowed_methods,
    reason = "bounds a real call to a node's log service"
)]
pub(super) async fn follow_from(
    connection: impl Into<NodeLogConnection>,
    request: NodeLogRequest<'_>,
    query: &DeploymentLogFollowQuery,
    present: StillPresent,
    deadline: Duration,
) -> Result<DeploymentLogStreams, DeploymentLogError> {
    let connection = connection.into();
    let message = FollowDeploymentLogsRequest {
        target: Some(request.target(&connection)),
        source_names: request.source_names(),
        resume_positions: query
            .resume_positions()
            .iter()
            .map(|(source_name, latest)| DeploymentLogResumePosition {
                source_name: source_name.clone(),
                latest_timestamp: Some(timestamp(*latest)),
            })
            .collect(),
    };
    let cancellation = query.cancellation().clone();
    let opening = async {
        let mut frames = connection.follow(message).await?;
        let first = frames.next().await.transpose()?;
        Ok::<_, Status>((frames, first))
    };
    let opened = tokio::select! {
        () = cancellation.cancelled() => {
            request.record_unavailable("follow", "cancelled");
            return Err(DeploymentLogError::NodeUnavailable);
        }
        opened = tokio::time::timeout(deadline, opening) => opened,
    };
    let (frames, first) = match opened {
        Ok(Ok(opened)) => opened,
        Ok(Err(status)) => {
            request.record_unavailable("follow", status_reason(&status));
            return Err(DeploymentLogError::NodeUnavailable);
        }
        Err(_) => {
            request.record_unavailable("follow", "deadline_exceeded");
            return Err(DeploymentLogError::NodeUnavailable);
        }
    };
    let openings = match first.and_then(|first| first.frame) {
        Some(follow_deployment_logs_response::Frame::Opened(opened)) => opened.sources,
        Some(follow_deployment_logs_response::Frame::Rejected(rejected)) => {
            // Unlike a read, a follow has no per-source answer to stand for a refusal: a
            // deployment the node has not installed may still be installed, so the follow
            // fails and its caller retries.
            request.rejected("follow", rejected.reason)?;
            return Err(DeploymentLogError::NodeUnavailable);
        }
        _ => {
            request.record_unavailable("follow", "protocol_violation");
            return Err(DeploymentLogError::NodeUnavailable);
        }
    };
    let sources = request.answered_sources(
        openings
            .iter()
            .filter_map(|opening| opening.source.as_ref())
            .map(|source| source.name.as_str()),
    );
    let mut streams = Vec::with_capacity(sources.len());
    let mut followed = BTreeMap::new();
    for source in &sources {
        let opening = openings.iter().find(|opening| {
            opening
                .source
                .as_ref()
                .is_some_and(|opened| opened.name == source.name)
        });
        let result = open_source(
            source,
            opening.map(|opening| opening.unavailable),
            &mut followed,
        );
        streams.push(RuntimeLogSourceStream {
            source: source.clone(),
            result,
        });
    }
    let (late, late_receiver) = match request.selection {
        NodeLogSelection::All { known, .. } => {
            let (sender, receiver) = mpsc::channel(known.len().max(1));
            let late = LateSources {
                sender,
                known: known.to_vec(),
            };
            (Some(late), Some(receiver))
        }
        NodeLogSelection::Named(_) => (None, None),
    };
    let failure = DeploymentLogFollowFailure::default();
    let demultiplexer = Demultiplexer {
        node_id: request.node_id.to_owned(),
        deployment_id: request.deployment_id.to_owned(),
        followed,
        closed: Vec::new(),
        late,
        present,
        failure: failure.clone(),
        cancellation,
    };
    tokio::spawn(demultiplexer.run(frames));
    Ok(DeploymentLogStreams {
        sources: streams,
        late: late_receiver,
        failure,
    })
}

/// The known sources a follow selecting every source may receive after it opened, and the
/// channel handing them to the management pipeline.
struct LateSources {
    sender: mpsc::Sender<RuntimeLogSourceStream>,
    known: Vec<RuntimeLogSource>,
}

/// Opens one source as its node reported it: followed when `opening` holds no unavailability.
fn open_source(
    source: &RuntimeLogSource,
    opening: Option<Option<i32>>,
    followed: &mut BTreeMap<String, FollowedSource>,
) -> Result<RuntimeLogStream, RuntimeBackendError> {
    match opening {
        Some(None) => {
            let (sender, receiver) = mpsc::channel(SOURCE_EVENT_BUFFER);
            let released = CancellationToken::new();
            let reading = released.clone().drop_guard();
            followed.insert(
                source.name.clone(),
                FollowedSource {
                    source: source.clone(),
                    sender,
                    released,
                },
            );
            let stream: RuntimeLogStream = Box::pin(
                tokio_stream::wrappers::ReceiverStream::new(receiver).map(move |event| {
                    // The reader holds the guard as long as the stream.
                    let _reading = &reading;
                    event
                }),
            );
            Ok(stream)
        }
        Some(Some(reason)) => Err(runtime_error(reason)),
        None => Err(RuntimeBackendError::remote_log_unavailable()),
    }
}

/// Routes the frames of one node follow to their source streams.
///
/// It runs until every source is done on a follow of named sources, until the node ends the
/// call, which on a follow of every source means the node no longer hosts the deployment, or
/// until the follow is cancelled. A broken call or protocol, or a deployment the manager still
/// wants running, as when it moved to another node, marks the follow failed before its source
/// streams end, so the caller reopens it. Sources that ended before the failure are read to
/// their end first, so they still read as ended. Returning drops the call, which stops the
/// node's read.
struct Demultiplexer {
    node_id: String,
    deployment_id: String,

    /// Each source currently open, by name.
    followed: BTreeMap<String, FollowedSource>,

    /// Release signals of the sources this follow closed whose reader may still hold them.
    closed: Vec<CancellationToken>,

    /// Present on a follow of every source.
    late: Option<LateSources>,
    present: StillPresent,
    failure: DeploymentLogFollowFailure,
    cancellation: CancellationToken,
}

impl Demultiplexer {
    async fn run(mut self, frames: NodeFollowFrames) {
        if let Some(reason) = self.route(frames).await {
            self.fail(reason).await;
        }
    }

    /// Routes frames until the follow is done, giving the failure reason when it broke. The call
    /// is dropped on return, before a failure waits on its readers.
    async fn route(&mut self, mut frames: NodeFollowFrames) -> Option<&'static str> {
        while !self.followed.is_empty() || self.late.is_some() {
            let message = tokio::select! {
                () = self.cancellation.cancelled() => return None,
                message = frames.next() => message,
            };
            let frame = match message {
                Some(Ok(FollowDeploymentLogsResponse { frame: Some(frame) })) => frame,
                Some(Ok(FollowDeploymentLogsResponse { frame: None })) => {
                    return Some("protocol_violation");
                }
                Some(Err(status)) => return Some(status_reason(&status)),
                // Only a follow of every source outlives its sources: its node ends it when it
                // stops hosting the deployment. Named sources still open were lost instead.
                None if self.late.is_some() && (self.present)() => {
                    return Some("deployment_left_node");
                }
                None if self.late.is_some() => {
                    debug!(
                        event_kind = "node_deployment_log_follow_ended",
                        node_id = %self.node_id,
                        deployment_id = %self.deployment_id,
                        "Node ended the deployment-log follow of a removed deployment"
                    );
                    return None;
                }
                None => return Some("ended_with_open_sources"),
            };
            let routed = match frame {
                follow_deployment_logs_response::Frame::Entry(entry) => self.entry(entry).await,
                follow_deployment_logs_response::Frame::SourceUnavailable(unavailable) => {
                    if let Some(followed) = self.close(&unavailable.source_name) {
                        let event = match unavailable.reason {
                            None => Ok(RuntimeLogEvent::SourceUnavailable(followed.source)),
                            Some(reason) => Err(runtime_error(reason)),
                        };
                        let _ = followed.sender.send(event).await;
                    }
                    Routed::Continue
                }
                // Dropping the sender ends the source's stream, which reads as an ended source.
                follow_deployment_logs_response::Frame::SourceEnded(ended) => {
                    self.close(&ended.source_name);
                    Routed::Continue
                }
                follow_deployment_logs_response::Frame::SourceOpened(opening) => {
                    self.source_opened(opening).await
                }
                follow_deployment_logs_response::Frame::Opened(_)
                | follow_deployment_logs_response::Frame::Rejected(_) => {
                    Routed::Failed("protocol_violation")
                }
            };
            match routed {
                Routed::Continue => {}
                Routed::Stop => return None,
                Routed::Failed(reason) => return Some(reason),
            }
        }
        None
    }

    async fn entry(&mut self, entry: DeploymentLogSourceEntry) -> Routed {
        // A source this follow stopped reading may still have entries in flight.
        let Some(followed) = self.followed.get(&entry.source_name) else {
            return Routed::Continue;
        };
        let event = entry
            .entry
            .and_then(|entry| runtime_entry(entry, &followed.source))
            .map(RuntimeLogEvent::Entry)
            .ok_or_else(RuntimeBackendError::remote_log_unavailable);
        let malformed = event.is_err();
        let sent = tokio::select! {
            () = self.cancellation.cancelled() => return Routed::Stop,
            sent = followed.sender.send(event) => sent.is_ok(),
        };
        if !sent || malformed {
            self.close(&entry.source_name);
        }
        Routed::Continue
    }

    /// Hands a source the node opened after the follow opened to the management pipeline: a
    /// known source not open yet, with its owner's label. The node opens a source again only
    /// after its terminal frame, so an opening for a source still open breaks the protocol.
    async fn source_opened(&mut self, opening: DeploymentLogSourceOpening) -> Routed {
        let Some(late) = self.late.as_ref() else {
            return Routed::Failed("protocol_violation");
        };
        let Some(source) = opening.source.as_ref().and_then(|opened| {
            late.known
                .iter()
                .find(|known| known.name == opened.name)
                .cloned()
        }) else {
            return Routed::Continue;
        };
        if self.followed.contains_key(&source.name) {
            return Routed::Failed("source_opened_twice");
        }
        let result = open_source(&source, Some(opening.unavailable), &mut self.followed);
        let sent = tokio::select! {
            () = self.cancellation.cancelled() => return Routed::Stop,
            sent = late.sender.send(RuntimeLogSourceStream { source, result }) => sent.is_ok(),
        };
        // The management pipeline stops reading only when the follow is going away.
        if sent { Routed::Continue } else { Routed::Stop }
    }

    /// Stops following a source, keeping its release signal until its reader lets it go.
    fn close(&mut self, source_name: &str) -> Option<FollowedSource> {
        let followed = self.followed.remove(source_name)?;
        self.closed.retain(|released| !released.is_cancelled());
        self.closed.push(followed.released.clone());
        Some(followed)
    }

    /// Marks the follow failed once the sources it closed earlier are read to their end, then
    /// ends every open source stream by dropping it.
    async fn fail(self, reason: &'static str) {
        warn!(
            event_kind = "node_deployment_log_follow_failed",
            node_id = %self.node_id,
            deployment_id = %self.deployment_id,
            reason,
            "Manager lost the deployment-log follow of a node"
        );
        // A source's stream ends the same way whether it ended or was cut short; its reader
        // tells them apart by the failure, so a source that ended must be read before it.
        let released =
            futures_util::future::join_all(self.closed.iter().map(CancellationToken::cancelled));
        tokio::select! {
            () = self.cancellation.cancelled() => {}
            _ = released => {}
        }
        self.failure.fail();
    }
}

/// One source the follow currently routes frames to.
struct FollowedSource {
    source: RuntimeLogSource,
    sender: SourceSender,

    /// Cancelled once the source's reader drops its stream.
    released: CancellationToken,
}

/// What routing one node frame leaves the follow to do.
enum Routed {
    Continue,

    /// The follow is going away, so nothing more is routed.
    Stop,

    /// The call broke, for the given reason.
    Failed(&'static str),
}

/// Whether `site` declares the deployment and wants it running: a deployment no longer
/// declared, or one being torn down or paused, is not.
fn desired_present(site: &Site, owner_kind: DeploymentOwnerKind, deployment_id: &str) -> bool {
    match owner_kind {
        DeploymentOwnerKind::Model => site.model_deployments.iter().any(|deployment| {
            deployment.model_deployment_id == deployment_id
                && deployment.desired_state == ModelDeploymentDesiredState::Present as i32
        }),
        DeploymentOwnerKind::Application => site.application_deployments.iter().any(|deployment| {
            deployment.application_deployment_id == deployment_id
                && deployment.desired_state == ApplicationDeploymentDesiredState::Present as i32
        }),
        DeploymentOwnerKind::Unspecified => false,
    }
}

fn adapt_histories(
    histories: DeploymentLogHistories,
    sources: &[RuntimeLogSource],
) -> Vec<RuntimeLogSourceHistory> {
    sources
        .iter()
        .map(|source| {
            let node = histories.sources.iter().find(|history| {
                history
                    .source
                    .as_ref()
                    .is_some_and(|node| node.name == source.name)
            });
            let result = match node.and_then(|history| history.result.clone()) {
                Some(deployment_log_source_history::Result::History(history)) => {
                    let received = history.entries.len();
                    let entries = history
                        .entries
                        .into_iter()
                        .filter_map(|entry| runtime_entry(entry, source))
                        .collect::<Vec<_>>();
                    Ok(RuntimeLogHistory {
                        // A malformed entry is dropped like one the budget dropped.
                        candidate_budget_truncated: history.candidate_budget_truncated
                            || entries.len() != received,
                        entries,
                        has_older_output: history.has_older_output,
                    })
                }
                Some(deployment_log_source_history::Result::Unavailable(reason)) => {
                    Err(runtime_error(reason))
                }
                None => Err(RuntimeBackendError::remote_log_unavailable()),
            };
            RuntimeLogSourceHistory {
                source: source.clone(),
                result,
            }
        })
        .collect()
}

fn unavailable_histories(sources: &[RuntimeLogSource]) -> Vec<RuntimeLogSourceHistory> {
    sources
        .iter()
        .map(|source| RuntimeLogSourceHistory {
            source: source.clone(),
            result: Err(RuntimeBackendError::remote_log_unavailable()),
        })
        .collect()
}

fn status_reason(status: &Status) -> &'static str {
    match status.code() {
        tonic::Code::Unauthenticated => "unauthenticated",
        tonic::Code::PermissionDenied => "permission_denied",
        tonic::Code::Unimplemented => "unimplemented",
        tonic::Code::Unavailable => "unavailable",
        tonic::Code::Internal => "node_internal",
        _ => "call_failed",
    }
}

fn client(channel: Channel) -> deployment_log_service_client::DeploymentLogServiceClient<Channel> {
    deployment_log_service_client::DeploymentLogServiceClient::new(channel)
        .max_decoding_message_size(MESSAGE_LIMIT_BYTES)
        .max_encoding_message_size(MESSAGE_LIMIT_BYTES)
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::deployment::private::v1::{
        DeploymentLogSource, DeploymentLogSourceEnded,
    };

    use super::*;

    #[tokio::test]
    async fn an_opening_for_a_source_still_open_fails_the_follow() {
        let model = RuntimeLogSource {
            name: "model".to_owned(),
            label: "Model".to_owned(),
        };
        let mut followed = BTreeMap::new();
        let open = open_source(&model, Some(None), &mut followed).expect("an open source");
        let (late_sender, mut late) = mpsc::channel(1);
        let failure = DeploymentLogFollowFailure::default();
        let demultiplexer = Demultiplexer {
            node_id: "worker-one".to_owned(),
            deployment_id: "deployment-a".to_owned(),
            followed,
            closed: Vec::new(),
            late: Some(LateSources {
                sender: late_sender,
                known: vec![model.clone()],
            }),
            present: Arc::new(|| false),
            failure: failure.clone(),
            cancellation: CancellationToken::new(),
        };
        let opening =
            follow_deployment_logs_response::Frame::SourceOpened(DeploymentLogSourceOpening {
                source: Some(DeploymentLogSource {
                    name: "model".to_owned(),
                    label: "Model".to_owned(),
                }),
                unavailable: None,
            });
        let frames = futures_util::stream::iter([Ok(FollowDeploymentLogsResponse {
            frame: Some(opening),
        })]);

        demultiplexer.run(Box::pin(frames)).await;

        assert!(failure.failed());
        assert!(open.collect::<Vec<_>>().await.is_empty());
        assert!(late.recv().await.is_none(), "the opening is refused");
    }

    #[tokio::test]
    async fn a_call_breaking_after_a_source_ended_fails_only_once_that_source_is_read() {
        let model = RuntimeLogSource {
            name: "model".to_owned(),
            label: "Model".to_owned(),
        };
        let mut followed = BTreeMap::new();
        let mut ended = open_source(&model, Some(None), &mut followed).expect("an open source");
        let failure = DeploymentLogFollowFailure::default();
        let demultiplexer = Demultiplexer {
            node_id: "worker-one".to_owned(),
            deployment_id: "deployment-a".to_owned(),
            followed,
            closed: Vec::new(),
            late: Some(LateSources {
                sender: mpsc::channel(1).0,
                known: vec![model.clone()],
            }),
            present: Arc::new(|| true),
            failure: failure.clone(),
            cancellation: CancellationToken::new(),
        };
        let frames = futures_util::stream::iter([
            Ok(FollowDeploymentLogsResponse {
                frame: Some(follow_deployment_logs_response::Frame::SourceEnded(
                    DeploymentLogSourceEnded {
                        source_name: "model".to_owned(),
                    },
                )),
            }),
            Err(Status::unavailable("connection broke")),
        ]);

        let routing = tokio::spawn(demultiplexer.run(Box::pin(frames)));
        let end = ended.next().await;
        let failed_at_end = failure.failed();
        drop(ended);
        routing.await.expect("routing");

        assert!(end.is_none(), "the ended source's stream ends");
        assert!(!failed_at_end, "the source ended before the follow failed");
        assert!(failure.failed());
    }

    #[test]
    fn only_a_declared_deployment_wanted_running_is_present() {
        use dotcore_proto::dotcore::{
            application::v1::ApplicationDeploymentDeclaration,
            model::v1::ModelDeploymentDeclaration,
        };

        let model = |id: &str, state: ModelDeploymentDesiredState| ModelDeploymentDeclaration {
            model_deployment_id: id.to_owned(),
            desired_state: state as i32,
            ..Default::default()
        };
        let application =
            |id: &str, state: ApplicationDeploymentDesiredState| ApplicationDeploymentDeclaration {
                application_deployment_id: id.to_owned(),
                desired_state: state as i32,
                ..Default::default()
            };
        let site = Site {
            model_deployments: vec![
                model("running", ModelDeploymentDesiredState::Present),
                model("tearing-down", ModelDeploymentDesiredState::Teardown),
                model("paused", ModelDeploymentDesiredState::Paused),
            ],
            application_deployments: vec![
                application("running", ApplicationDeploymentDesiredState::Present),
                application("tearing-down", ApplicationDeploymentDesiredState::Teardown),
            ],
            ..Default::default()
        };

        let present = |owner_kind, id: &str| desired_present(&site, owner_kind, id);

        assert!(present(DeploymentOwnerKind::Model, "running"));
        assert!(!present(DeploymentOwnerKind::Model, "tearing-down"));
        assert!(!present(DeploymentOwnerKind::Model, "paused"));
        assert!(!present(DeploymentOwnerKind::Model, "removed"));
        assert!(present(DeploymentOwnerKind::Application, "running"));
        assert!(!present(DeploymentOwnerKind::Application, "tearing-down"));
        assert!(
            !present(DeploymentOwnerKind::Application, "paused"),
            "an undeclared application is not present"
        );
    }
}
