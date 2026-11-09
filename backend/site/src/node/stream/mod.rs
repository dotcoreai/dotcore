//! Serves the node control stream on the site manager: one long-lived exchange per node process
//! that carries the node's observation, controller status and view acknowledgements up and its
//! desired views down.
//!
//! Every node message passes identity checks, incarnation fences and an owner commit section, and
//! is answered once, in order, with its acceptance or a typed
//! rejection. Any message is contact, a closed stream disconnects its node at once while serving
//! keeps the silence bound, and a newer stream of the same node process replaces the older one.
//! The node's view is published on open, after each site change that alters it, and again on a
//! fixed interval, which renews the node's action lease and is the manager's heartbeat.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_domain::{clock::Clock, timing::Timing};
use dotcore_proto::dotcore::site::private::v1::{
    NodeControlMessageKind, NodeControlOpened, NodeControlRejection, NodeControlRejectionCode,
    NodeControllerStatus, NodeDesiredView, NodeObservation, NodeViewAcknowledgement,
    StreamNodeControlRequest, StreamNodeControlResponse,
    node_control_service_server::{NodeControlService, NodeControlServiceServer},
    stream_node_control_request, stream_node_control_response,
};
use futures_util::{
    StreamExt,
    stream::{self, BoxStream},
};
use tokio::{sync::Notify, time::Instant};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, info, warn};

use crate::{
    node::{
        control::ManagerNodeControlCoordinator,
        registry::{NodeRegistry, NodeRegistryError},
    },
    private_transport::{
        AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext, run_commit_section,
    },
    view::{
        contract::validate_stream_open,
        desired::{DesiredViewDistributionError, ManagerNodeViewPublisher, rejected_exchange},
        fencing::{IncarnationFenceError, NodeIncarnation},
    },
};

/// Default number of replies a stream queues before its node is considered not to read them.
const DEFAULT_OUTBOUND_REPLIES: usize = 32;

/// Publications a node misses before it reopens its stream, by default; the action lease must
/// outlast them plus one reconnect.
const MISSED_PUBLICATIONS_BEFORE_REOPEN: u32 = 3;

const _: () = assert!(
    Timing::production().view_republish_interval.as_millis()
        * MISSED_PUBLICATIONS_BEFORE_REOPEN as u128
        + Timing::production()
            .node_stream_retry_backoff_max
            .as_millis()
        < Timing::production().node_action_lease.as_millis()
);

/// The site manager's node control streams, one per node process, each message served through
/// its owner's commit section.
///
/// Remote nodes reach it through [`NodeControlStreamService`]; the manager's own node serves its
/// stream in process through [`Self::serve`].
#[derive(Clone)]
pub struct ManagerNodeControlStreams {
    inner: Arc<StreamsInner>,
}

impl ManagerNodeControlStreams {
    /// Creates the manager's streams over the node registry and desired-view publisher that share
    /// one node control coordinator, timing waits on that coordinator's clock.
    pub fn new(
        registry: NodeRegistry,
        publisher: ManagerNodeViewPublisher,
        configuration: NodeControlStreamConfiguration,
    ) -> Self {
        let coordinator = publisher.coordinator().clone();
        Self {
            inner: Arc::new(StreamsInner {
                clock: coordinator.clock(),
                coordinator,
                registry,
                publisher,
                configuration,
            }),
        }
    }

    /// Wraps the streams as the private receiver of remote worker nodes.
    pub fn private_service(self, context: &PrivateServiceContext) -> NodeControlStreamService {
        NodeControlStreamService {
            streams: self,
            authorization: context.authorize_role(PrivateNodeRole::Worker),
        }
    }

    /// Serves one stream of the `SiteNode` `node_id` of site `site_id`, whose identity the caller
    /// has already established, answering the node's `inbound` messages on the returned stream.
    ///
    /// The exchange runs on its own task until the node closes its side, the stream fails, or the
    /// returned stream is dropped.
    pub fn serve(
        &self,
        site_id: String,
        node_id: String,
        inbound: BoxStream<'static, Result<StreamNodeControlRequest, Status>>,
    ) -> BoxStream<'static, Result<StreamNodeControlResponse, Status>> {
        let outbound = Arc::new(Outbound::new(self.inner.configuration.outbound_replies));
        let disconnected = CancellationToken::new();
        let session = StreamSession {
            streams: Arc::clone(&self.inner),
            site_id,
            node_id,
            outbound: Arc::clone(&outbound),
            disconnected: disconnected.clone(),
        };
        tokio::spawn(session.run(inbound));
        // Dropping the returned stream, as the transport does when the node goes away, ends the
        // exchange.
        let disconnect_on_drop = disconnected.drop_guard();
        stream::unfold(
            (outbound, disconnect_on_drop),
            |(outbound, disconnect_on_drop)| async move {
                let message = outbound.next().await?;
                Some((message, (outbound, disconnect_on_drop)))
            },
        )
        .boxed()
    }
}

/// Timing and queue bounds of the manager's node control streams. The silence bound is the
/// coordinator's, so a silent stream ends exactly when its node stops being live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeControlStreamConfiguration {
    republish_interval: Duration,
    outbound_replies: usize,
}

impl NodeControlStreamConfiguration {
    /// Republishes an unchanged view at the republish interval of `timing` and queues at most
    /// `outbound_replies` unsent replies per stream.
    ///
    /// The interval must let a node that misses three publications, then reconnects after its
    /// longest retry pause, receive a new view before its action lease expires.
    pub fn new(
        timing: &Timing,
        outbound_replies: usize,
    ) -> Result<Self, NodeControlConfigurationError> {
        let republish_interval = timing.view_republish_interval;
        let reopen_bound = republish_interval
            .checked_mul(MISSED_PUBLICATIONS_BEFORE_REOPEN)
            .and_then(|missed| missed.checked_add(timing.node_stream_retry_backoff_max));
        if republish_interval.is_zero()
            || reopen_bound.is_none_or(|bound| bound >= timing.node_action_lease)
        {
            return Err(NodeControlConfigurationError::RepublishInterval);
        }
        if outbound_replies == 0 {
            return Err(NodeControlConfigurationError::OutboundReplies);
        }
        Ok(Self {
            republish_interval,
            outbound_replies,
        })
    }

    /// Keeps this configuration's reply bound and follows `timing` instead, under the same rule
    /// as [`Self::new`].
    pub fn with_timing(self, timing: &Timing) -> Result<Self, NodeControlConfigurationError> {
        Self::new(timing, self.outbound_replies)
    }
}

impl Default for NodeControlStreamConfiguration {
    fn default() -> Self {
        Self {
            republish_interval: Timing::production().view_republish_interval,
            outbound_replies: DEFAULT_OUTBOUND_REPLIES,
        }
    }
}

/// An invalid node control stream configuration.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NodeControlConfigurationError {
    #[error("the view republish interval must be nonzero and keep nodes inside their action lease")]
    RepublishInterval,

    #[error("node control streams must queue at least one reply")]
    OutboundReplies,
}

/// Private gRPC receiver of remote worker node control streams.
pub struct NodeControlStreamService {
    streams: ManagerNodeControlStreams,
    authorization: AuthenticatedRoleAuthorization,
}

impl NodeControlStreamService {
    /// Wraps the service for route assembly.
    pub fn into_server(self) -> NodeControlServiceServer<Self> {
        NodeControlServiceServer::new(self)
    }
}

#[tonic::async_trait]
impl NodeControlService for NodeControlStreamService {
    type StreamNodeControlStream = BoxStream<'static, Result<StreamNodeControlResponse, Status>>;

    async fn stream_node_control(
        &self,
        request: Request<Streaming<StreamNodeControlRequest>>,
    ) -> Result<Response<Self::StreamNodeControlStream>, Status> {
        if self.authorization.receiver_role() != PrivateNodeRole::Manager {
            return Err(Status::permission_denied(
                "node control streams are accepted only by the site manager",
            ));
        }
        let identity = self.authorization.authorize(&request)?;
        let inbound = request.into_inner().boxed();
        Ok(Response::new(self.streams.serve(
            identity.site_id,
            identity.node_id,
            inbound,
        )))
    }
}

struct StreamsInner {
    coordinator: ManagerNodeControlCoordinator,
    registry: NodeRegistry,
    publisher: ManagerNodeViewPublisher,
    clock: Arc<dyn Clock>,
    configuration: NodeControlStreamConfiguration,
}

/// One served stream of one authenticated node.
struct StreamSession {
    streams: Arc<StreamsInner>,
    site_id: String,
    node_id: String,
    outbound: Arc<Outbound>,
    disconnected: CancellationToken,
}

impl StreamSession {
    /// Serves the stream to its end, records the close for liveness, then ends the outbound side,
    /// so a node that sees its stream end already reads as disconnected.
    async fn run(self, mut inbound: BoxStream<'static, Result<StreamNodeControlRequest, Status>>) {
        let end = match self.open(&mut inbound).await {
            Ok(mut stream) => {
                let end = self.exchange(&mut stream, &mut inbound).await;
                self.close(&stream).await;
                end
            }
            Err(end) => end,
        };
        match &end {
            StreamEnd::Failed(status) => info!(
                event_kind = "node_control_stream_ended",
                node_id = self.node_id,
                code = ?status.code(),
                reason = status.message(),
                "Manager ended a node control stream"
            ),
            StreamEnd::NodeClosed | StreamEnd::Disconnected => info!(
                event_kind = "node_control_stream_ended",
                node_id = self.node_id,
                reason = "node closed the stream",
                "Node control stream closed"
            ),
        }
        self.outbound.finish(match end {
            StreamEnd::Failed(status) => Some(status),
            StreamEnd::NodeClosed | StreamEnd::Disconnected => None,
        });
    }

    /// Waits for the node's open within the silence bound, attaches the stream to its node and
    /// answers with the manager session and the node's current view.
    async fn open(
        &self,
        inbound: &mut BoxStream<'static, Result<StreamNodeControlRequest, Status>>,
    ) -> Result<OpenedStream, StreamEnd> {
        let deadline = self.streams.clock.now() + self.streams.coordinator.silence_bound();
        let message = tokio::select! {
            biased;
            () = self.disconnected.cancelled() => return Err(StreamEnd::Disconnected),
            message = inbound.next() => message,
            () = self.streams.clock.sleep_until(deadline) => {
                return Err(StreamEnd::Failed(Status::deadline_exceeded(
                    "node control stream sent no open within the silence bound",
                )));
            }
        };
        let open = match message {
            None | Some(Err(_)) => return Err(StreamEnd::NodeClosed),
            Some(Ok(StreamNodeControlRequest {
                message: Some(stream_node_control_request::Message::Open(open)),
            })) => open,
            Some(Ok(_)) => {
                return Err(StreamEnd::Failed(Status::invalid_argument(
                    "node control stream must start with open",
                )));
            }
        };
        if open.site_id != self.site_id || open.node_id != self.node_id {
            return Err(StreamEnd::Failed(Status::permission_denied(
                "node control stream open does not match the authenticated node",
            )));
        }
        let incarnation = validate_stream_open(&open)
            .and_then(|()| Ok(NodeIncarnation::parse(open.node_incarnation_id.clone())?))
            .map_err(|_| {
                StreamEnd::Failed(Status::invalid_argument(
                    "node control stream open is invalid or incomplete",
                ))
            })?;

        // Registered before the first view is projected, so no later site change is missed.
        let site_changed = self.streams.publisher.site_change_listener();
        let publisher = self.streams.publisher.clone();
        let node_id = self.node_id.clone();
        let opened_incarnation = incarnation.clone();
        let (attachment, view) =
            run_commit_section(move || publisher.commit_stream_open(&node_id, &opened_incarnation))
                .await
                .map_err(StreamEnd::Failed)?
                .map_err(|error| match error {
                    DesiredViewDistributionError::Incarnation(
                        IncarnationFenceError::StreamReplaced,
                    ) => replaced(),
                    error => {
                        StreamEnd::Failed(rejected_exchange(&self.node_id, "stream open", error))
                    }
                })?;
        let revision = view_revision(&view);
        info!(
            event_kind = "node_control_stream_opened",
            node_id = self.node_id,
            node_incarnation = incarnation.as_str(),
            stream_epoch = attachment.epoch,
            view_revision = revision,
            "Node control stream opened"
        );
        let stream = OpenedStream {
            incarnation,
            epoch: attachment.epoch,
            replaced: attachment.replaced,
            site_changed,
            next_publication: 2,
            published_revision: revision,
            republish_at: self.streams.clock.now() + self.streams.configuration.republish_interval,
        };
        self.reply(stream_node_control_response::Message::Opened(
            NodeControlOpened {
                manager_session_id: self
                    .streams
                    .coordinator
                    .manager_session()
                    .as_str()
                    .to_owned(),
                view: Some(view),
            },
        ))?;
        Ok(stream)
    }

    /// Answers node messages and publishes views until the stream ends.
    async fn exchange(
        &self,
        stream: &mut OpenedStream,
        inbound: &mut BoxStream<'static, Result<StreamNodeControlRequest, Status>>,
    ) -> StreamEnd {
        let silence_bound = self.streams.coordinator.silence_bound();
        let mut silent_after = self.streams.clock.now() + silence_bound;
        loop {
            let epoch = stream.epoch;
            let event = tokio::select! {
                biased;
                () = self.disconnected.cancelled() => Event::Disconnected,
                _ = stream.replaced.wait_for(|latest| *latest != epoch) => Event::Replaced,
                message = inbound.next() => match message {
                    Some(Ok(message)) => Event::Message(Box::new(message)),
                    None | Some(Err(_)) => Event::NodeClosed,
                },
                () = self.streams.clock.sleep_until(silent_after) => Event::Silent,
                () = stream.site_changed.notified() => Event::SiteChanged,
                () = self.streams.clock.sleep_until(stream.republish_at) => Event::Republish,
            };
            let outcome = match event {
                Event::Disconnected => Err(StreamEnd::Disconnected),
                Event::Replaced => Err(replaced()),
                Event::Silent => Err(StreamEnd::Failed(Status::deadline_exceeded(
                    "node control stream was silent past the silence bound",
                ))),
                Event::NodeClosed => Err(StreamEnd::NodeClosed),
                Event::Message(message) => {
                    silent_after = self.streams.clock.now() + silence_bound;
                    self.answer(stream, *message).await
                }
                Event::SiteChanged => self.publish(stream, false).await,
                Event::Republish => self.publish(stream, true).await,
            };
            if let Err(end) = outcome {
                return end;
            }
        }
    }

    /// Answers one node message after the open, ending the stream on an identity mismatch.
    async fn answer(
        &self,
        stream: &OpenedStream,
        message: StreamNodeControlRequest,
    ) -> Result<(), StreamEnd> {
        match message.message {
            Some(stream_node_control_request::Message::Observation(observation)) => {
                self.observation(stream, observation).await
            }
            Some(stream_node_control_request::Message::Status(status)) => {
                self.status(stream, status).await
            }
            Some(stream_node_control_request::Message::AcknowledgeView(acknowledgement)) => {
                self.acknowledgement(stream, acknowledgement).await
            }
            Some(stream_node_control_request::Message::Heartbeat(_)) => {
                let coordinator = self.streams.coordinator.clone();
                let node_id = self.node_id.clone();
                let incarnation = stream.incarnation.clone();
                let epoch = stream.epoch;
                // A commit section that cannot run leaves contact to the next message.
                let _ = run_commit_section(move || {
                    coordinator.stream_contact(&node_id, &incarnation, epoch);
                })
                .await;
                Ok(())
            }
            Some(stream_node_control_request::Message::Open(_)) => Err(StreamEnd::Failed(
                Status::invalid_argument("node control stream opens only once"),
            )),
            None => Err(StreamEnd::Failed(Status::invalid_argument(
                "node control message sets no payload",
            ))),
        }
    }

    async fn observation(
        &self,
        stream: &OpenedStream,
        request: NodeObservation,
    ) -> Result<(), StreamEnd> {
        if request.site_id != self.site_id || request.node_id != self.node_id {
            return Err(StreamEnd::Failed(Status::permission_denied(
                "node observation identity does not match the authenticated node",
            )));
        }
        if request.incarnation_id != stream.incarnation.as_str() {
            return Err(StreamEnd::Failed(Status::failed_precondition(
                "node observation names another incarnation than its stream",
            )));
        }
        let revision = request.revision;
        let registry = self.streams.registry.clone();
        let coordinator = self.streams.coordinator.clone();
        let node_id = self.node_id.clone();
        let incarnation = stream.incarnation.clone();
        let epoch = stream.epoch;
        let accepted = run_commit_section(move || {
            coordinator.stream_contact(&node_id, &incarnation, epoch);
            registry.accept_on_stream(request, epoch)
        })
        .await;
        let status = match accepted {
            Ok(Ok(response)) => {
                return self.reply(stream_node_control_response::Message::ObservationAccepted(
                    response,
                ));
            }
            Ok(Err(error)) => {
                let status = registry_status(&error);
                debug!(event_kind = "node_observation_acceptance", outcome = "rejected", node_id = self.node_id, revision, error = %error, "Node observation was rejected");
                status
            }
            Err(status) => status,
        };
        if status.code() == tonic::Code::PermissionDenied {
            return Err(StreamEnd::Failed(status));
        }
        self.reject(NodeControlMessageKind::Observation, revision, &status)
    }

    async fn status(
        &self,
        stream: &OpenedStream,
        request: NodeControllerStatus,
    ) -> Result<(), StreamEnd> {
        let sequence = request.status_sequence;
        let Some(reference) = request.view.as_ref() else {
            return self.reject(
                NodeControlMessageKind::Status,
                sequence,
                &Status::invalid_argument("controller status has no accepted view"),
            );
        };
        self.check_view_reference(
            stream,
            &reference.site_id,
            &reference.node_id,
            &reference.node_incarnation_id,
            &reference.manager_session_id,
        )?;
        let publisher = self.streams.publisher.clone();
        let node_id = self.node_id.clone();
        let incarnation = stream.incarnation.clone();
        let epoch = stream.epoch;
        let accepted = run_commit_section(move || {
            publisher
                .coordinator()
                .stream_contact(&node_id, &incarnation, epoch);
            publisher.commit_status(&node_id, &request)
        })
        .await;
        match self.committed(accepted, "status") {
            Ok(response) => self.reply(stream_node_control_response::Message::StatusAccepted(
                response,
            )),
            Err(status) => self.reject(NodeControlMessageKind::Status, sequence, &status),
        }
    }

    async fn acknowledgement(
        &self,
        stream: &OpenedStream,
        request: NodeViewAcknowledgement,
    ) -> Result<(), StreamEnd> {
        let sequence = request.acknowledgement_sequence;
        let Some(reference) = request.view.as_ref() else {
            return self.reject(
                NodeControlMessageKind::Acknowledgement,
                sequence,
                &Status::invalid_argument("desired-view acknowledgement has no view"),
            );
        };
        self.check_view_reference(
            stream,
            &reference.site_id,
            &reference.node_id,
            &reference.node_incarnation_id,
            &reference.manager_session_id,
        )?;
        let publisher = self.streams.publisher.clone();
        let node_id = self.node_id.clone();
        let incarnation = stream.incarnation.clone();
        let epoch = stream.epoch;
        let accepted = run_commit_section(move || {
            publisher
                .coordinator()
                .stream_contact(&node_id, &incarnation, epoch);
            publisher.commit_acknowledgement(&node_id, &request)
        })
        .await;
        match self.committed(accepted, "acknowledgement") {
            Ok(response) => {
                self.reply(stream_node_control_response::Message::AcknowledgementAccepted(response))
            }
            Err(status) => self.reject(NodeControlMessageKind::Acknowledgement, sequence, &status),
        }
    }

    /// Ends the stream when a view reference names another node than the authenticated one
    /// (permission denied), or another incarnation or manager session than the stream's.
    fn check_view_reference(
        &self,
        stream: &OpenedStream,
        site_id: &str,
        node_id: &str,
        node_incarnation_id: &str,
        manager_session_id: &str,
    ) -> Result<(), StreamEnd> {
        if site_id != self.site_id || node_id != self.node_id {
            return Err(StreamEnd::Failed(Status::permission_denied(
                "node control message identity does not match the authenticated node",
            )));
        }
        if node_incarnation_id != stream.incarnation.as_str()
            || manager_session_id != self.streams.coordinator.manager_session().as_str()
        {
            return Err(StreamEnd::Failed(Status::failed_precondition(
                "node control message names another incarnation or manager session than its stream",
            )));
        }
        Ok(())
    }

    /// Projects a commit section's outcome to its reply or to the status that classifies its
    /// rejection.
    fn committed<T>(
        &self,
        outcome: Result<Result<T, DesiredViewDistributionError>, Status>,
        exchange: &'static str,
    ) -> Result<T, Status> {
        outcome?.map_err(|error| rejected_exchange(&self.node_id, exchange, error))
    }

    /// Publishes the node's current view: always on a republish, and on a site change only when
    /// its revision changed. A failed publication is retried at the next republish.
    async fn publish(&self, stream: &mut OpenedStream, always: bool) -> Result<(), StreamEnd> {
        let publisher = self.streams.publisher.clone();
        let node_id = self.node_id.clone();
        let incarnation = stream.incarnation.clone();
        let epoch = stream.epoch;
        let sequence = stream.next_publication;
        let published = run_commit_section(move || {
            publisher.commit_stream_publication(&node_id, &incarnation, epoch, sequence)
        })
        .await;
        let view = match published {
            Ok(Ok(view)) => view,
            Ok(Err(DesiredViewDistributionError::Incarnation(
                IncarnationFenceError::StreamReplaced,
            ))) => return Err(replaced()),
            Ok(Err(error)) => {
                warn!(event_kind = "node_control_view_publication", outcome = "failed", node_id = self.node_id, error = %error, "Manager could not publish a node view; it retries at the next republish");
                stream.republish_at =
                    self.streams.clock.now() + self.streams.configuration.republish_interval;
                return Ok(());
            }
            Err(status) => {
                warn!(event_kind = "node_control_view_publication", outcome = "failed", node_id = self.node_id, error = %status, "Manager could not publish a node view; it retries at the next republish");
                stream.republish_at =
                    self.streams.clock.now() + self.streams.configuration.republish_interval;
                return Ok(());
            }
        };
        let revision = view_revision(&view);
        if !always && revision == stream.published_revision {
            return Ok(());
        }
        if revision != stream.published_revision {
            debug!(
                event_kind = "node_control_view_publication",
                outcome = "changed",
                node_id = self.node_id,
                view_revision = revision,
                publication_sequence = sequence,
                "Manager published a changed node view"
            );
        }
        stream.next_publication = sequence
            .checked_add(1)
            .expect("node view publication sequence exhausted");
        stream.published_revision = revision;
        stream.republish_at =
            self.streams.clock.now() + self.streams.configuration.republish_interval;
        self.outbound.publish(view);
        Ok(())
    }

    /// Records the stream's close for liveness; a replaced stream's close changes nothing.
    async fn close(&self, stream: &OpenedStream) {
        let coordinator = self.streams.coordinator.clone();
        let node_id = self.node_id.clone();
        let incarnation = stream.incarnation.clone();
        let epoch = stream.epoch;
        let closed = {
            let coordinator = coordinator.clone();
            let node_id = node_id.clone();
            let incarnation = incarnation.clone();
            run_commit_section(move || coordinator.close_stream(&node_id, &incarnation, epoch))
                .await
        };
        // The close only takes the coordinator lock, so it still runs here when the commit
        // section could not, rather than leaving the node connected.
        let closed = closed.unwrap_or_else(|status| {
            warn!(event_kind = "node_control_stream_closed", outcome = "inline", node_id = self.node_id, error = %status, "Manager recorded a node control stream close outside a commit section");
            coordinator.close_stream(&node_id, &incarnation, epoch)
        });
        debug!(
            event_kind = "node_control_stream_closed",
            node_id = self.node_id,
            stream_epoch = stream.epoch,
            disconnected = closed,
            "Manager recorded a node control stream close"
        );
    }

    fn reject(
        &self,
        kind: NodeControlMessageKind,
        sequence: u64,
        status: &Status,
    ) -> Result<(), StreamEnd> {
        let code = match status.code() {
            tonic::Code::InvalidArgument => NodeControlRejectionCode::Invalid,
            tonic::Code::FailedPrecondition => NodeControlRejectionCode::Conflict,
            _ => NodeControlRejectionCode::Unavailable,
        };
        self.reply(stream_node_control_response::Message::Rejected(
            NodeControlRejection {
                kind: kind as i32,
                sequence,
                code: code as i32,
            },
        ))
    }

    /// Queues one reply, ending the stream when the node stopped reading them.
    fn reply(&self, message: stream_node_control_response::Message) -> Result<(), StreamEnd> {
        self.outbound.reply(StreamNodeControlResponse {
            message: Some(message),
        })
    }
}

/// One stream after its open was accepted.
struct OpenedStream {
    incarnation: NodeIncarnation,
    epoch: u64,
    replaced: tokio::sync::watch::Receiver<u64>,
    site_changed: Arc<Notify>,
    next_publication: u64,
    published_revision: u64,
    republish_at: Instant,
}

/// What woke a stream.
enum Event {
    Disconnected,
    Replaced,
    Message(Box<StreamNodeControlRequest>),
    NodeClosed,
    Silent,
    SiteChanged,
    Republish,
}

/// How a stream ended.
#[derive(Debug)]
enum StreamEnd {
    /// The node closed its side or its transport broke.
    NodeClosed,

    /// The transport stopped reading the manager's side.
    Disconnected,

    /// The manager ended the stream with this status.
    Failed(Status),
}

/// Messages waiting for the transport: replies in order, never dropped, and at most one pending
/// view, which a newer publication replaces.
struct Outbound {
    state: Mutex<OutboundState>,
    ready: Notify,
    capacity: usize,
}

#[derive(Default)]
struct OutboundState {
    replies: VecDeque<StreamNodeControlResponse>,
    view: Option<NodeDesiredView>,
    end: Option<Option<Status>>,
    finished: bool,
}

impl Outbound {
    fn new(capacity: usize) -> Self {
        Self {
            state: Mutex::default(),
            ready: Notify::new(),
            capacity,
        }
    }

    /// Queues one reply, failing when the queue is full.
    fn reply(&self, message: StreamNodeControlResponse) -> Result<(), StreamEnd> {
        {
            let mut state = self.state.lock().expect("node control outbound lock");
            if state.replies.len() >= self.capacity {
                return Err(StreamEnd::Failed(Status::resource_exhausted(
                    "node control stream replies are not being read",
                )));
            }
            state.replies.push_back(message);
        }
        self.ready.notify_one();
        Ok(())
    }

    /// Replaces any unsent view with `view`.
    fn publish(&self, view: NodeDesiredView) {
        self.state.lock().expect("node control outbound lock").view = Some(view);
        self.ready.notify_one();
    }

    /// Ends the outbound side after its queued replies, with `status` when the manager failed the
    /// stream; an unsent view is dropped.
    fn finish(&self, status: Option<Status>) {
        {
            let mut state = self.state.lock().expect("node control outbound lock");
            state.view = None;
            state.end = Some(status);
        }
        self.ready.notify_one();
    }

    /// Waits for the next message to send; `None` once the stream ended.
    async fn next(&self) -> Option<Result<StreamNodeControlResponse, Status>> {
        loop {
            {
                let mut state = self.state.lock().expect("node control outbound lock");
                if let Some(reply) = state.replies.pop_front() {
                    return Some(Ok(reply));
                }
                if let Some(view) = state.view.take() {
                    return Some(Ok(StreamNodeControlResponse {
                        message: Some(stream_node_control_response::Message::ViewPublished(view)),
                    }));
                }
                if state.finished {
                    return None;
                }
                if let Some(end) = state.end.take() {
                    state.finished = true;
                    match end {
                        Some(status) => return Some(Err(status)),
                        None => return None,
                    }
                }
            }
            self.ready.notified().await;
        }
    }
}

/// Projects one registry rejection to the status that classifies the node's observation: permission
/// denied ends the stream, any other status rejects only that observation.
fn registry_status(error: &NodeRegistryError) -> Status {
    match error {
        NodeRegistryError::SiteIdentityMismatch | NodeRegistryError::UndeclaredNode => {
            Status::permission_denied("node observation is not authorized for this node")
        }
        NodeRegistryError::InvalidNodeIdentity(_)
        | NodeRegistryError::InvalidIncarnation
        | NodeRegistryError::InvalidRevision
        | NodeRegistryError::InvalidReport(_) => {
            Status::invalid_argument("node observation is invalid or incomplete")
        }
        NodeRegistryError::ConfigurationDigestMismatch
        | NodeRegistryError::StaleRevision { .. }
        | NodeRegistryError::CurrentIncarnationFresh
        | NodeRegistryError::ReplacedIncarnation => {
            Status::failed_precondition("node observation is stale or conflicts with current state")
        }
        NodeRegistryError::Site(_) | NodeRegistryError::Configuration(_) => {
            Status::internal("node observation validation failed")
        }
    }
}

fn replaced() -> StreamEnd {
    StreamEnd::Failed(Status::aborted(
        "a newer node control stream of this node replaced this one",
    ))
}

fn view_revision(view: &NodeDesiredView) -> u64 {
    view.identity
        .as_ref()
        .map_or(0, |identity| identity.view_revision)
}

#[cfg(test)]
mod tests;
