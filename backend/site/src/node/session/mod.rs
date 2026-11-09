//! Holds a node's side of its node control stream to the site manager: one long-lived exchange
//! that carries the node's observations, controller status and view acknowledgements up, and
//! installs the desired views the manager publishes down.
//!
//! Every open sends the node's identity, waits for the manager's answer, re-sends the latest
//! observation, installs the published view and only then reports status, so each new stream
//! restores the manager's picture of the node. The node sends a heartbeat while it has nothing
//! else to send, and reopens with backoff when the stream ends or the manager stays silent past a
//! bound. The same exchange runs over private transport from a node process, or in process for
//! the node that shares the manager process; every wait reads the backend clock.

use std::{collections::BTreeSet, future::Future, pin::Pin, sync::Arc, time::Duration};

use anyhow::Context as _;
use dotcore_domain::{
    clock::{Clock, next_boundary},
    timing::{Backoff, NodeControlTiming},
};
use dotcore_proto::dotcore::{
    application::private::v1::ApplicationCleanupDisposition,
    model::private::v1::ModelCleanupDisposition,
    site::private::v1::{
        ControllerStatusSection, DesiredViewRejection, DesiredViewSectionKind,
        NodeControlMessageKind, NodeControlOpen, NodeControlRejection, NodeControlRejectionCode,
        NodeControllerStatus, NodeControllerStatusAcceptance, NodeDesiredView, NodeObservation,
        NodeViewAcknowledgement, NodeViewReference, StreamNodeControlRequest,
        StreamNodeControlResponse, SupportedDesiredViewSection, controller_status_receipt,
        controller_status_section, node_control_service_client::NodeControlServiceClient,
        node_view_acknowledgement, stream_node_control_request, stream_node_control_response,
    },
};
use futures_util::{StreamExt, future::BoxFuture, stream::BoxStream};
use tokio::sync::{mpsc, watch};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Status,
    transport::{Channel, Endpoint},
};
use tracing::{debug, info, warn};

use super::{
    agent::{ManagerChannelClosed, ObservationReportSink, within},
    hardware_inventory::HardwareInventorySource,
    stream::ManagerNodeControlStreams,
};
use crate::{
    store::FileNodeState,
    view::{
        contract::{APPLICATION_SECTION_SCHEMA_VERSION, MODEL_SECTION_SCHEMA_VERSION},
        desired::{
            DesiredViewDistributionError, NodeApplicationStatusOwner, NodeModelStatusOwner,
            NodeViewInstaller, rejection_code,
        },
        fencing::{AcceptedNodeViewAuthority, ManagerSession, NodeViewStream, ViewAcceptance},
    },
};

/// Messages the node queues toward its stream before it waits for the transport.
const OUTBOUND_MESSAGES: usize = 16;

/// One node process's control exchange with the site manager, reopened for as long as it runs.
///
/// Observation, status and acknowledgement sequences belong to the node incarnation, so they keep
/// increasing across reopened streams.
pub struct NodeControlSession {
    open: NodeControlOpen,
    transport: Arc<dyn NodeControlTransport>,
    authority: AcceptedNodeViewAuthority,
    installer: NodeViewInstaller,
    observations: NodeControlObservations,

    /// The node's hardware collections; each status pins one for every section it attributes.
    inventory: Arc<dyn HardwareInventorySource>,

    state: FileNodeState,
    model: Arc<dyn NodeModelStatusOwner>,
    application: Option<Arc<dyn NodeApplicationStatusOwner>>,
    /// Heartbeat, manager silence, status slots and retry pauses of the stream.
    timing: NodeControlTiming,
    clock: Arc<dyn Clock>,
}

impl NodeControlSession {
    /// Creates the exchange of the `SiteNode` `node_id` of site `site_id` over `transport`. Views
    /// install through `installer` into `authority`, status is built from the owners with
    /// accelerator memory attributed from one collection of `inventory`, and the latest
    /// observation comes from `observations`.
    ///
    /// The node sends a heartbeat after an idle heartbeat interval, and reopens the stream after
    /// the manager stays silent past its silence, which also bounds the wait for the manager's
    /// answer to an open. A changed status is looked for at every wall-aligned check slot, and
    /// triggered statuses go out at the end of the current wall-aligned coalesce window.
    #[expect(
        clippy::too_many_arguments,
        reason = "each argument is a distinct node-lifetime dependency injected by composition"
    )]
    pub fn new(
        site_id: impl Into<String>,
        node_id: impl Into<String>,
        transport: Arc<dyn NodeControlTransport>,
        authority: AcceptedNodeViewAuthority,
        installer: NodeViewInstaller,
        observations: NodeControlObservations,
        inventory: Arc<dyn HardwareInventorySource>,
        state: FileNodeState,
        model: Arc<dyn NodeModelStatusOwner>,
        application: Option<Arc<dyn NodeApplicationStatusOwner>>,
        timing: NodeControlTiming,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let open = NodeControlOpen {
            site_id: site_id.into(),
            node_id: node_id.into(),
            node_incarnation_id: authority.node_incarnation().as_str().to_owned(),
            supported_sections: supported_sections(),
        };
        Self {
            open,
            transport,
            authority,
            installer,
            observations,
            inventory,
            state,
            model,
            application,
            timing,
            clock,
        }
    }

    /// Starts the exchange; its first stream opens once the node has an observation to send.
    pub fn start(self) -> NodeControlSessionTask {
        NodeControlSessionTask {
            task: tokio::spawn(dotcore_domain::background_loop::log_unexpected_loop_end(
                "node_control_session",
                self.run(),
            )),
        }
    }

    /// Opens streams one after another, pausing with growing backoff after each failed open and
    /// briefly after each stream that ended.
    async fn run(self) {
        let (serving_changed, serving_changes) = watch::channel(0_u64);
        self.model.register_change_observer(Arc::new(move || {
            serving_changed.send_modify(|count| *count = count.wrapping_add(1));
        }));
        let mut triggers = Triggers {
            observations: self.observations.latest(),
            serving_changes,
            state_changes: self.state.subscribe_changes(),
        };
        // The manager registers an incarnation from its observation, so nothing opens before it.
        if triggers
            .observations
            .wait_for(Option::is_some)
            .await
            .is_err()
        {
            return;
        }
        let mut cursors = ExchangeCursors {
            status_sequence: 1,
            acknowledgement_sequence: 1,
            status_rejected: false,
        };
        let mut backoff = RetryBackoff::new(self.timing.stream_retry);
        loop {
            match self.serve_stream(&mut cursors, &mut triggers).await {
                StreamOutcome::NotOpened => {}
                StreamOutcome::Ended => {
                    backoff = RetryBackoff::new(self.timing.stream_retry);
                }
                StreamOutcome::Stopped => return,
            }
            backoff.wait(self.clock.as_ref()).await;
        }
    }

    /// Opens one stream and serves it to its end.
    async fn serve_stream(
        &self,
        cursors: &mut ExchangeCursors,
        triggers: &mut Triggers,
    ) -> StreamOutcome {
        let (sender, receiver) = mpsc::channel(OUTBOUND_MESSAGES);
        sender
            .try_send(control_message(stream_node_control_request::Message::Open(
                self.open.clone(),
            )))
            .expect("a new outbound queue has room for the open");
        let opened = within(
            self.clock.as_ref(),
            self.timing.manager_silence,
            self.open_stream(receiver),
        )
        .await
        .unwrap_or_else(|| {
            Err(Status::deadline_exceeded(
                "manager did not answer the node control open in time",
            ))
        });
        let (inbound, manager_session_id, view) = match opened {
            Ok(opened) => opened,
            Err(status) => {
                warn!(event_kind = "node_control_stream_open", outcome = "failed", code = ?status.code(), error = %dotcore_domain::diagnostic::diagnostic_chain(&status), "Node could not open its control stream to the manager");
                return StreamOutcome::NotOpened;
            }
        };
        let stream = match ManagerSession::parse(manager_session_id)
            .map_err(anyhow::Error::new)
            .and_then(|session| {
                self.authority
                    .open_stream(session)
                    .map_err(anyhow::Error::new)
            }) {
            Ok(stream) => stream,
            Err(error) => {
                warn!(event_kind = "node_control_stream_open", outcome = "rejected", error = %error, "Node refused the manager session that answered its control stream");
                return StreamOutcome::NotOpened;
            }
        };
        info!(
            event_kind = "node_control_stream_opened",
            outcome = "success",
            manager_session = stream.manager_session().as_str(),
            "Node opened its control stream to the manager"
        );
        let now = self.clock.now();
        let mut link = StreamLink {
            sender,
            stream,
            last_sent: now,
            last_received: now,
            next_status_check: self.aligned_deadline(self.timing.status_check_interval),
            status_due: None,
            status_in_flight: None,
            status_wanted: false,
            status_follows_answer: false,
            last_status: None,
        };
        self.observations.set_connected(true);
        let end = self
            .exchange(&mut link, inbound, view, cursors, triggers)
            .await;
        self.observations.set_connected(false);
        match &end {
            StreamEnd::Failed(status) => {
                warn!(event_kind = "node_control_stream_ended", code = ?status.code(), status_message = status.message(), "Manager ended the node control stream; the node reopens it")
            }
            StreamEnd::Closed => warn!(
                event_kind = "node_control_stream_ended",
                reason = "closed",
                "Node control stream closed; the node reopens it"
            ),
            StreamEnd::ManagerSilent => warn!(
                event_kind = "node_control_stream_ended",
                reason = "manager_silent",
                "Manager was silent past the node's bound; the node reopens its control stream"
            ),
            StreamEnd::Protocol(reason) => warn!(
                event_kind = "node_control_stream_ended",
                reason,
                "Manager broke the node control protocol; the node reopens its control stream"
            ),
            StreamEnd::Stopped => {}
        }
        match end {
            StreamEnd::Stopped => StreamOutcome::Stopped,
            _ => StreamOutcome::Ended,
        }
    }

    /// Opens the transport and waits for the manager's answer to the open.
    async fn open_stream(
        &self,
        outbound: mpsc::Receiver<StreamNodeControlRequest>,
    ) -> Result<(ManagerMessages, String, Option<NodeDesiredView>), Status> {
        let mut inbound = self.transport.open(outbound).await?;
        match inbound.next().await {
            Some(Ok(StreamNodeControlResponse {
                message: Some(stream_node_control_response::Message::Opened(opened)),
            })) => Ok((inbound, opened.manager_session_id, opened.view)),
            Some(Ok(_)) => Err(Status::internal(
                "manager answered the node control open with another message",
            )),
            Some(Err(status)) => Err(status),
            None => Err(Status::unavailable(
                "manager closed the node control stream before answering its open",
            )),
        }
    }

    /// Re-sends the node's state in the stated order, then answers manager messages and node
    /// changes until the stream ends.
    async fn exchange(
        &self,
        link: &mut StreamLink,
        mut inbound: ManagerMessages,
        view: Option<NodeDesiredView>,
        cursors: &mut ExchangeCursors,
        triggers: &mut Triggers,
    ) -> StreamEnd {
        let latest = triggers.observations.borrow_and_update().clone();
        if let Some(observation) = latest
            && let Err(end) = self.send_observation(link, observation).await
        {
            return end;
        }
        let Some(view) = view else {
            return StreamEnd::Protocol("the open answer carries no view");
        };
        if let Err(end) = self.publication(link, view, cursors, true).await {
            return end;
        }
        loop {
            let heartbeat_at = link.last_sent + self.timing.heartbeat_interval;
            let silent_at = link.last_received + self.timing.manager_silence;
            let event = tokio::select! {
                biased;
                message = inbound.next() => match message {
                    Some(Ok(message)) => Event::Message(Box::new(message)),
                    Some(Err(status)) => return StreamEnd::Failed(status),
                    None => return StreamEnd::Closed,
                },
                changed = triggers.observations.changed() => match changed {
                    Ok(()) => Event::Observation,
                    Err(_) => return StreamEnd::Stopped,
                },
                changed = triggers.serving_changes.changed() => match changed {
                    Ok(()) => Event::StatusTrigger,
                    Err(_) => return StreamEnd::Stopped,
                },
                changed = triggers.state_changes.changed() => match changed {
                    Ok(()) => Event::StatusTrigger,
                    Err(_) => return StreamEnd::Stopped,
                },
                () = self.clock.sleep_until(heartbeat_at) => Event::Heartbeat,
                () = self.clock.sleep_until(link.next_status_check) => Event::StatusCheck,
                () = self.status_due(link.status_due) => Event::StatusDue,
                () = self.clock.sleep_until(silent_at) => return StreamEnd::ManagerSilent,
            };
            let handled = match event {
                Event::Message(message) => {
                    link.last_received = self.clock.now();
                    self.answer(link, *message, cursors).await
                }
                Event::Observation => {
                    let latest = triggers.observations.borrow_and_update().clone();
                    match latest {
                        Some(observation) => {
                            self.observation_with_status(link, observation, cursors)
                                .await
                        }
                        None => Ok(()),
                    }
                }
                Event::StatusTrigger => {
                    // One status covers every trigger that arrived before it was built.
                    triggers.serving_changes.borrow_and_update();
                    self.request_status(link);
                    Ok(())
                }
                Event::StatusCheck => {
                    // The check runs at a wall boundary, so a change it finds goes out at once.
                    link.next_status_check =
                        self.aligned_deadline(self.timing.status_check_interval);
                    if self.status_changed(link) {
                        self.send_status(link, cursors).await
                    } else {
                        Ok(())
                    }
                }
                Event::StatusDue => {
                    link.status_due = None;
                    if link.status_wanted {
                        self.send_status(link, cursors).await
                    } else {
                        Ok(())
                    }
                }
                Event::Heartbeat => {
                    self.send(
                        link,
                        stream_node_control_request::Message::Heartbeat(
                            dotcore_proto::google::protobuf::Empty {},
                        ),
                    )
                    .await
                }
            };
            if let Err(end) = handled {
                return end;
            }
        }
    }

    /// Acts on one manager message after the open.
    async fn answer(
        &self,
        link: &mut StreamLink,
        message: StreamNodeControlResponse,
        cursors: &mut ExchangeCursors,
    ) -> Result<(), StreamEnd> {
        match message.message {
            Some(stream_node_control_response::Message::ViewPublished(view)) => {
                self.publication(link, view, cursors, false).await
            }
            Some(stream_node_control_response::Message::ObservationAccepted(response)) => {
                self.observations.answer(response.accepted_revision, None);
                Ok(())
            }
            Some(stream_node_control_response::Message::StatusAccepted(response)) => {
                let Some(request) = link.status_in_flight.take() else {
                    return Err(StreamEnd::Protocol("manager accepted a status never sent"));
                };
                if response.accepted_status_sequence != request.status_sequence {
                    return Err(StreamEnd::Protocol(
                        "manager accepted another status than the one sent",
                    ));
                }
                if let Err(error) = self.accept_status_receipts(&request, &response) {
                    warn!(event_kind = "node_model_status", outcome = "receipt_not_recorded", status_sequence = request.status_sequence, error = %error, "Node could not record manager cleanup receipts; the next status reports the unacknowledged evidence again");
                }
                if cursors.status_rejected {
                    info!(
                        event_kind = "node_model_status",
                        outcome = "recovered",
                        "Manager accepted node model status after an earlier rejection"
                    );
                }
                cursors.status_rejected = false;
                if std::mem::take(&mut link.status_follows_answer) && self.status_changed(link) {
                    return self.send_status(link, cursors).await;
                }
                self.resend_wanted_status(link);
                Ok(())
            }
            Some(stream_node_control_response::Message::AcknowledgementAccepted(response)) => {
                debug!(
                    event_kind = "node_desired_view_acknowledgement",
                    outcome = "accepted",
                    acknowledgement_sequence = response.accepted_acknowledgement_sequence,
                    "Manager accepted a desired-view acknowledgement"
                );
                Ok(())
            }
            Some(stream_node_control_response::Message::Rejected(rejection)) => {
                self.rejected(link, rejection, cursors).await
            }
            Some(stream_node_control_response::Message::Opened(_)) => {
                Err(StreamEnd::Protocol("manager answered the open twice"))
            }
            None => Err(StreamEnd::Protocol("manager message sets no payload")),
        }
    }

    /// Records one rejected node message; a rejected status is rebuilt only after a later trigger.
    async fn rejected(
        &self,
        link: &mut StreamLink,
        rejection: NodeControlRejection,
        cursors: &mut ExchangeCursors,
    ) -> Result<(), StreamEnd> {
        let code = NodeControlRejectionCode::try_from(rejection.code)
            .unwrap_or(NodeControlRejectionCode::Unspecified);
        match NodeControlMessageKind::try_from(rejection.kind) {
            Ok(NodeControlMessageKind::Observation) => {
                self.observations.answer(rejection.sequence, Some(code));
                Ok(())
            }
            Ok(NodeControlMessageKind::Status) => {
                if link
                    .status_in_flight
                    .take_if(|request| request.status_sequence == rejection.sequence)
                    .is_none()
                {
                    return Err(StreamEnd::Protocol("manager rejected a status never sent"));
                }
                if cursors.status_rejected {
                    debug!(event_kind = "node_model_status", outcome = "rejected", code = ?code, "Manager rejected rebuilt node model status");
                } else {
                    warn!(event_kind = "node_model_status", outcome = "rejected", code = ?code, "Manager rejected node model status; it is rebuilt after the next change or publication");
                }
                cursors.status_rejected = true;
                link.status_follows_answer = false;
                self.resend_wanted_status(link);
                Ok(())
            }
            Ok(NodeControlMessageKind::Acknowledgement) => {
                debug!(event_kind = "node_desired_view_acknowledgement", outcome = "rejected", acknowledgement_sequence = rejection.sequence, code = ?code, "Node discarded a rejected desired-view acknowledgement");
                Ok(())
            }
            Ok(NodeControlMessageKind::Unspecified) | Err(_) => Err(StreamEnd::Protocol(
                "manager rejected an unknown message kind",
            )),
        }
    }

    /// Installs one published view, acknowledges its outcome, and reports status for an accepted
    /// one, which also keeps the manager's status fresh while the view is unchanged. The status
    /// goes out at once when `immediate` (the open's), otherwise at the next wall-second boundary.
    async fn publication(
        &self,
        link: &mut StreamLink,
        view: NodeDesiredView,
        cursors: &mut ExchangeCursors,
        immediate: bool,
    ) -> Result<(), StreamEnd> {
        let revision = view
            .identity
            .as_ref()
            .map_or(0, |identity| identity.view_revision);
        let installed = self
            .installer
            .install_publication(&self.open, &link.stream, &view);
        let outcome = match &installed {
            Ok(acceptance) => {
                if *acceptance == ViewAcceptance::Installed {
                    info!(
                        event_kind = "node_desired_view_installed",
                        outcome = "success",
                        view_revision = revision,
                        "Node accepted complete desired view"
                    );
                }
                node_view_acknowledgement::Outcome::Accepted(
                    dotcore_proto::google::protobuf::Empty {},
                )
            }
            Err(error) => {
                let code = rejection_code(error);
                warn!(event_kind = "node_desired_view_rejected", outcome = "rejected", view_revision = revision, rejection_code = ?code, error = %error, "Node rejected complete desired view");
                node_view_acknowledgement::Outcome::Rejected(DesiredViewRejection {
                    code: code as i32,
                })
            }
        };
        if let Some(acknowledgement) =
            acknowledgement_for(&view, cursors.acknowledgement_sequence, outcome)
        {
            cursors.acknowledgement_sequence = cursors
                .acknowledgement_sequence
                .checked_add(1)
                .expect("desired-view acknowledgement sequence exhausted");
            self.send(
                link,
                stream_node_control_request::Message::AcknowledgeView(acknowledgement),
            )
            .await?;
        }
        if installed.is_ok() {
            if immediate {
                self.send_status(link, cursors).await?;
            } else {
                self.request_status(link);
            }
        }
        Ok(())
    }

    /// Marks a status wanted and schedules it for the next wall-second boundary, so every trigger
    /// before that boundary shares one message and at most one status is sent per second.
    fn request_status(&self, link: &mut StreamLink) {
        link.status_wanted = true;
        if link.status_due.is_none() {
            link.status_due = Some(self.aligned_deadline(self.timing.status_coalesce_window));
        }
    }

    /// Sends one complete status for the installed view, or marks one wanted while another is
    /// unanswered, so at most one status is ever in flight.
    async fn send_status(
        &self,
        link: &mut StreamLink,
        cursors: &mut ExchangeCursors,
    ) -> Result<(), StreamEnd> {
        if link.status_in_flight.is_some() {
            link.status_wanted = true;
            return Ok(());
        }
        link.status_wanted = false;
        self.model.retire_withdrawn_cleanup_evidence();
        if let Some(application) = &self.application {
            application.retire_withdrawn_cleanup_evidence();
        }
        let Some(request) = self.status_request(cursors.status_sequence) else {
            return Ok(());
        };
        cursors.status_sequence = cursors
            .status_sequence
            .checked_add(1)
            .expect("controller status sequence exhausted");
        link.status_in_flight = Some(request.clone());
        link.last_status = Some((request.view.clone(), request.sections.clone()));
        self.send(link, stream_node_control_request::Message::Status(request))
            .await
    }

    /// Deadline of the next wall-clock multiple of `interval` on the session clock, so every
    /// node's status goes out on the same boundaries.
    fn aligned_deadline(&self, interval: Duration) -> tokio::time::Instant {
        let wall = self.clock.wall_now();
        let wait = next_boundary(wall, interval)
            .duration_since(wall)
            .unwrap_or_default();
        self.clock.now() + wait
    }

    /// Waits for `due`, or forever when no status is scheduled.
    async fn status_due(&self, due: Option<tokio::time::Instant>) {
        match due {
            Some(due) => self.clock.sleep_until(due).await,
            None => std::future::pending().await,
        }
    }

    /// Whether the status the node would report now differs from the last one sent on `link`.
    fn status_changed(&self, link: &StreamLink) -> bool {
        let Some(current) = self.status_request(0) else {
            return false;
        };
        link.last_status
            .as_ref()
            .is_none_or(|(view, sections)| *view != current.view || *sections != current.sections)
    }

    /// Schedules the status a trigger asked for while the previous one was unanswered.
    fn resend_wanted_status(&self, link: &mut StreamLink) {
        if link.status_wanted {
            self.request_status(link);
        }
    }

    /// Sends one new observation, then the status right after it when that changed, so
    /// attribution computed from the same observation reaches the manager with it rather than at
    /// the next check. While another status is unanswered, the changed one goes out on its answer.
    async fn observation_with_status(
        &self,
        link: &mut StreamLink,
        observation: NodeObservation,
        cursors: &mut ExchangeCursors,
    ) -> Result<(), StreamEnd> {
        self.send_observation(link, observation).await?;
        if !self.status_changed(link) {
            return Ok(());
        }
        if link.status_in_flight.is_some() {
            link.status_follows_answer = true;
            return Ok(());
        }
        self.send_status(link, cursors).await
    }

    async fn send_observation(
        &self,
        link: &mut StreamLink,
        observation: NodeObservation,
    ) -> Result<(), StreamEnd> {
        self.send(
            link,
            stream_node_control_request::Message::Observation(observation),
        )
        .await
    }

    /// Queues one message toward the manager; a transport that stopped reading ends the stream.
    async fn send(
        &self,
        link: &mut StreamLink,
        message: stream_node_control_request::Message,
    ) -> Result<(), StreamEnd> {
        // Recorded before the message is queued, so the heartbeat deadline never trails it.
        link.last_sent = self.clock.now();
        link.sender
            .send(control_message(message))
            .await
            .map_err(|_| StreamEnd::Closed)
    }

    /// Builds one complete status for the installed view, with every section attributing
    /// accelerator memory from the same hardware collection, pinned once for the whole build.
    /// Unacknowledged cleanup evidence stands in for its teardown; every other assignment,
    /// including a teardown whose evidence was already acknowledged, reports its own status.
    fn status_request(&self, status_sequence: u64) -> Option<NodeControllerStatus> {
        let view = self.authority.accepted_view_reference()?;
        let collection = self.inventory.collection();
        let model = self.model.status_section(&collection)?;
        let mut sections = vec![ControllerStatusSection {
            payload: Some(controller_status_section::Payload::Model(model)),
        }];
        if let Some(application) = self
            .application
            .as_ref()
            .and_then(|application| application.status_section(&collection))
        {
            sections.push(ControllerStatusSection {
                payload: Some(controller_status_section::Payload::Application(application)),
            });
        }
        Some(NodeControllerStatus {
            view: Some(view),
            status_sequence,
            sections,
        })
    }

    /// Marks every acknowledged entry so it is never reported again; rejected entries stay
    /// retained and are reported by a later status.
    fn accept_status_receipts(
        &self,
        request: &NodeControllerStatus,
        response: &NodeControllerStatusAcceptance,
    ) -> Result<(), DesiredViewDistributionError> {
        crate::view::contract::validate_status_receipts(request, response)?;
        let mut acknowledged = BTreeSet::new();
        let mut acknowledged_applications = BTreeSet::new();
        for receipt in &response.receipts {
            match receipt.payload.as_ref() {
                Some(controller_status_receipt::Payload::Model(receipt)) => {
                    if receipt.disposition == ModelCleanupDisposition::Acknowledged as i32 {
                        acknowledged.insert((
                            receipt.model_deployment_id.clone(),
                            receipt.lifecycle_generation,
                        ));
                    }
                }
                Some(controller_status_receipt::Payload::Application(receipt)) => {
                    if receipt.disposition == ApplicationCleanupDisposition::Acknowledged as i32 {
                        acknowledged_applications.insert((
                            receipt.application_deployment_id.clone(),
                            receipt.lifecycle_generation,
                        ));
                    }
                }
                None => {
                    return Err(DesiredViewDistributionError::Contract(
                        crate::view::contract::DesiredViewContractError::ReceiptMismatch,
                    ));
                }
            }
        }
        self.model.accept_receipts(&acknowledged)?;
        if let Some(application) = &self.application
            && !acknowledged_applications.is_empty()
        {
            application.accept_receipts(&acknowledged_applications)?;
        }
        Ok(())
    }
}

/// Running node control exchange, aborted when backend serving stops.
pub struct NodeControlSessionTask {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for NodeControlSessionTask {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The node's latest observation, shared between its reporter and its control stream: the stream
/// sends each new one and re-sends the latest on every open.
///
/// As a report sink it waits for the manager's answer on the open stream, and fails at once while
/// no stream is open; the observation is still sent when the next stream opens.
#[derive(Clone, Default)]
pub struct NodeControlObservations {
    shared: Arc<ObservationShared>,
}

impl NodeControlObservations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `request` as the latest observation without waiting for an answer, for one the
    /// manager already accepted another way.
    pub(crate) fn seed(&self, request: NodeObservation) {
        self.shared.latest.send_replace(Some(request));
    }

    fn latest(&self) -> watch::Receiver<Option<NodeObservation>> {
        self.shared.latest.subscribe()
    }

    fn answer(&self, revision: u64, rejection: Option<NodeControlRejectionCode>) {
        self.shared.answers.send_replace(Some(ObservationAnswer {
            revision,
            rejection,
        }));
    }

    fn set_connected(&self, connected: bool) {
        self.shared.connected.send_replace(connected);
    }

    /// The latest observation the stream sends next.
    #[cfg(test)]
    pub(crate) fn latest_request(&self) -> Option<NodeObservation> {
        self.shared.latest.borrow().clone()
    }
}

impl ObservationReportSink for NodeControlObservations {
    fn report(
        &self,
        request: NodeObservation,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let revision = request.revision;
            // Subscribed before the observation is offered, so its answer cannot be missed.
            let mut answers = self.shared.answers.subscribe();
            let mut connected = self.shared.connected.subscribe();
            self.shared.latest.send_replace(Some(request));
            loop {
                if !*connected.borrow_and_update() {
                    return Err(ManagerChannelClosed.into());
                }
                tokio::select! {
                    changed = answers.changed() => {
                        changed.context("node observation answers ended")?;
                        let answer = *answers.borrow_and_update();
                        match answer {
                            Some(ObservationAnswer { revision: answered, rejection: None })
                                if answered == revision => return Ok(()),
                            Some(ObservationAnswer { revision: answered, rejection: Some(code) })
                                if answered == revision => anyhow::bail!(
                                    "manager rejected node observation revision {revision}: {code:?}"
                                ),
                            _ => {}
                        }
                    }
                    changed = connected.changed() => {
                        changed.context("node control stream state ended")?;
                    }
                }
            }
        })
    }
}

/// Manager messages of one open node control stream.
pub type ManagerMessages = BoxStream<'static, Result<StreamNodeControlResponse, Status>>;

/// Carries a node's control streams to the site manager.
pub trait NodeControlTransport: Send + Sync {
    /// Opens one stream sending the messages queued on `outbound` to the manager and returns the
    /// manager's messages.
    fn open(
        &self,
        outbound: mpsc::Receiver<StreamNodeControlRequest>,
    ) -> BoxFuture<'_, Result<ManagerMessages, Status>>;
}

/// Authenticated node control transport over one reused private channel to the manager, which
/// connects on first use and reconnects on a later stream after a lost connection.
pub struct ManagerNodeControlClient {
    client: NodeControlServiceClient<Channel>,
}

impl ManagerNodeControlClient {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            client: NodeControlServiceClient::new(endpoint.connect_lazy()),
        }
    }
}

impl NodeControlTransport for ManagerNodeControlClient {
    fn open(
        &self,
        outbound: mpsc::Receiver<StreamNodeControlRequest>,
    ) -> BoxFuture<'_, Result<ManagerMessages, Status>> {
        let mut client = self.client.clone();
        Box::pin(async move {
            Ok(client
                .stream_node_control(ReceiverStream::new(outbound))
                .await?
                .into_inner()
                .boxed())
        })
    }
}

/// Node control streams of the `SiteNode` that shares the manager process, served by the
/// manager's streams in process with the node identity fixed at construction.
///
/// They pass the same checks, fences and commit sections as a remote node's stream, without a
/// private listener or site trust, and end only when the manager process stops.
pub struct InProcessNodeControlTransport {
    site_id: String,
    node_id: String,
    streams: ManagerNodeControlStreams,
}

impl InProcessNodeControlTransport {
    pub fn new(site_id: String, node_id: String, streams: ManagerNodeControlStreams) -> Self {
        Self {
            site_id,
            node_id,
            streams,
        }
    }
}

impl NodeControlTransport for InProcessNodeControlTransport {
    fn open(
        &self,
        outbound: mpsc::Receiver<StreamNodeControlRequest>,
    ) -> BoxFuture<'_, Result<ManagerMessages, Status>> {
        let inbound = ReceiverStream::new(outbound).map(Ok).boxed();
        let messages = self
            .streams
            .serve(self.site_id.clone(), self.node_id.clone(), inbound);
        Box::pin(std::future::ready(Ok(messages)))
    }
}

/// The open stream's sending side and its exchange state.
struct StreamLink {
    sender: mpsc::Sender<StreamNodeControlRequest>,
    stream: NodeViewStream,
    last_sent: tokio::time::Instant,
    last_received: tokio::time::Instant,
    next_status_check: tokio::time::Instant,

    /// When the coalesced status goes out, while some trigger awaits it.
    status_due: Option<tokio::time::Instant>,
    status_in_flight: Option<NodeControllerStatus>,
    status_wanted: bool,

    /// Whether a status an observation changed goes out as soon as the unanswered one is accepted.
    status_follows_answer: bool,

    /// View and sections of the last status sent on this stream.
    last_status: Option<(Option<NodeViewReference>, Vec<ControllerStatusSection>)>,
}

/// Sequences and outcomes that outlive one stream.
struct ExchangeCursors {
    status_sequence: u64,
    acknowledgement_sequence: u64,
    status_rejected: bool,
}

/// Node-side signals the open stream follows.
struct Triggers {
    observations: watch::Receiver<Option<NodeObservation>>,
    serving_changes: watch::Receiver<u64>,
    state_changes: watch::Receiver<u64>,
}

#[derive(Default)]
struct ObservationShared {
    latest: watch::Sender<Option<NodeObservation>>,
    answers: watch::Sender<Option<ObservationAnswer>>,
    connected: watch::Sender<bool>,
}

/// The manager's answer to one observation revision.
#[derive(Clone, Copy)]
struct ObservationAnswer {
    revision: u64,
    rejection: Option<NodeControlRejectionCode>,
}

/// What woke an open stream.
enum Event {
    Message(Box<StreamNodeControlResponse>),
    Observation,
    StatusTrigger,
    StatusCheck,
    StatusDue,
    Heartbeat,
}

/// How one open stream ended.
enum StreamEnd {
    /// The manager ended the stream with this status.
    Failed(Status),

    /// The stream closed without a status, or the transport stopped reading.
    Closed,

    /// The manager sent nothing within the node's bound.
    ManagerSilent,

    /// The manager sent a message the protocol does not allow.
    Protocol(&'static str),

    /// A node-side signal ended, so the node is shutting down.
    Stopped,
}

/// How one attempt to serve a stream ended.
enum StreamOutcome {
    NotOpened,
    Ended,
    Stopped,
}

/// Exponential pause between failed opens, bounded by the longest retry pause.
struct RetryBackoff {
    next: Duration,
    max: Duration,
}

impl RetryBackoff {
    fn new(backoff: Backoff) -> Self {
        Self {
            next: backoff.initial,
            max: backoff.max,
        }
    }

    async fn wait(&mut self, clock: &dyn Clock) {
        clock.sleep(self.next).await;
        self.next = (self.next * 2).min(self.max);
    }
}

fn control_message(message: stream_node_control_request::Message) -> StreamNodeControlRequest {
    StreamNodeControlRequest {
        message: Some(message),
    }
}

/// Section schema ranges this node understands.
fn supported_sections() -> Vec<SupportedDesiredViewSection> {
    vec![
        SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Model as i32,
            minimum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
            maximum_schema_version: MODEL_SECTION_SCHEMA_VERSION,
        },
        SupportedDesiredViewSection {
            kind: DesiredViewSectionKind::Application as i32,
            minimum_schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
            maximum_schema_version: APPLICATION_SECTION_SCHEMA_VERSION,
        },
    ]
}

/// Acknowledges the node's `outcome` for one published view; `None` when the view names no
/// identity to acknowledge.
fn acknowledgement_for(
    view: &NodeDesiredView,
    acknowledgement_sequence: u64,
    outcome: node_view_acknowledgement::Outcome,
) -> Option<NodeViewAcknowledgement> {
    let identity = view.identity.as_ref()?;
    Some(NodeViewAcknowledgement {
        view: Some(NodeViewReference {
            site_id: identity.site_id.clone(),
            node_id: identity.node_id.clone(),
            manager_session_id: identity.manager_session_id.clone(),
            node_incarnation_id: identity.node_incarnation_id.clone(),
            view_revision: identity.view_revision,
            content_sha256: identity.content_sha256.clone(),
        }),
        acknowledgement_sequence,
        outcome: Some(outcome),
    })
}

#[cfg(test)]
mod tests;
