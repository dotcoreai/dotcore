//! Serves management diagnostics for the whole site: the retained backend logs of every SiteNode,
//! merged into one timeline with each event tagged by its machine.
//!
//! History reads every selected machine in parallel and merges their pages by timestamp; each
//! machine keeps its own opaque position, so paging older never skips or repeats a machine's
//! events. Follow streams every selected machine; an unreachable or unavailable machine is
//! reported once and then no longer followed, while the others continue. Nothing is retained.

use std::{
    collections::{BTreeMap, HashMap},
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};

use dotcore_proto::dotcore::diagnostic::{
    private::v1::{
        FollowNodeDiagnosticsRequest, NodeDiagnosticHistory, NodeDiagnosticTarget,
        ReadNodeDiagnosticsRequest, follow_node_diagnostics_response::Frame,
        node_diagnostic_service_client::NodeDiagnosticServiceClient,
        read_node_diagnostics_response,
    },
    v1::{
        BackendDiagnosticEvent, DiagnosticHistoryContinuation, DiagnosticHistoryState,
        DiagnosticSourceState, FollowBackendDiagnosticsRequest, FollowBackendDiagnosticsResponse,
        MachineDiagnosticEvent, MachineDiagnosticPosition, MachineDiagnosticState,
        ReadBackendDiagnosticsRequest, ReadBackendDiagnosticsResponse,
        diagnostic_service_server::DiagnosticService as DiagnosticServiceContract,
        follow_backend_diagnostics_response::Event,
    },
};
use dotcore_site::{
    node::control::{CoordinatorConnectionState, ManagerNodeControlCoordinator},
    repository::SiteRepository,
    view::fencing::{ManagerSession, NodeIncarnation},
    worker_channels::{WorkerChannels, WorkerConnection},
};
use futures_util::{Stream, StreamExt, future::BoxFuture};
use prost::Message;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status, transport::Channel};
use tracing::debug;

use super::{
    MAX_RESPONSE_BYTES, MESSAGE_LIMIT_BYTES, NodeDiagnosticReader, node::NodeFollowFrames,
    resolve_page_size, retained::CancellableDiagnosticStream,
};
use crate::management_logs::NODE_LOG_DEADLINE;

/// Concurrent site history reads; each fans out to every selected machine.
const MAX_HISTORY_READS: usize = 4;

/// Concurrent follows of one machine; a follow over the limit reports the machine unavailable.
const MAX_MACHINE_FOLLOWS: usize = 8;

/// Events and states buffered toward the caller before machine follows wait for it.
const FOLLOW_BUFFER: usize = 256;

/// Public management `DiagnosticService` over every SiteNode's retained backend logs.
#[derive(Clone)]
pub(crate) struct SiteDiagnosticService {
    machines: Arc<dyn MachineDiagnostics>,
    histories: Arc<Semaphore>,
    follows: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
}

impl SiteDiagnosticService {
    /// Creates the service reading each machine through `machines`.
    pub(crate) fn new(machines: impl MachineDiagnostics + 'static) -> Self {
        Self {
            machines: Arc::new(machines),
            histories: Arc::new(Semaphore::new(MAX_HISTORY_READS)),
            follows: Arc::default(),
        }
    }

    /// Reserves one follow of `machine_id`; the permit is held for the follow's lifetime.
    fn acquire_follow(&self, machine_id: &str) -> Option<OwnedSemaphorePermit> {
        let machine = Arc::clone(
            self.follows
                .lock()
                .expect("diagnostic follow admission lock")
                .entry(machine_id.to_owned())
                .or_insert_with(|| Arc::new(Semaphore::new(MAX_MACHINE_FOLLOWS))),
        );
        machine.try_acquire_owned().ok()
    }
}

#[tonic::async_trait]
impl DiagnosticServiceContract for SiteDiagnosticService {
    async fn read_backend_diagnostics(
        &self,
        request: Request<ReadBackendDiagnosticsRequest>,
    ) -> Result<Response<ReadBackendDiagnosticsResponse>, Status> {
        let request = request.into_inner();
        let _permit = Arc::clone(&self.histories)
            .try_acquire_owned()
            .map_err(|_| capacity_exhausted())?;
        let page_size = resolve_page_size(request.page_size).ok_or_else(|| {
            Status::invalid_argument(
                "Diagnostic page size must be from 1 through 200, or 0 for the default",
            )
        })?;
        let newest = request.before.is_none();
        let declared = self.machines.declared()?;
        let reads = match request.before {
            None => declared
                .into_iter()
                .map(|machine| (machine, None))
                .collect(),
            Some(before) => {
                let mut positions = unique_positions(before.positions)?;
                declared
                    .into_iter()
                    .filter_map(|machine| {
                        let position = positions.remove(&machine.id)?;
                        Some((machine, Some(position)))
                    })
                    .collect::<Vec<_>>()
            }
        };
        let histories = futures_util::future::join_all(reads.iter().map(|(machine, before)| {
            self.machines
                .read(&machine.id, page_size as u32, before.clone())
        }))
        .await;
        let machines = reads
            .into_iter()
            .zip(histories)
            .map(|((machine, _), history)| MachineHistory { machine, history })
            .collect();
        Ok(Response::new(merge_histories(page_size, machines, newest)))
    }

    type FollowBackendDiagnosticsStream =
        Pin<Box<dyn Stream<Item = Result<FollowBackendDiagnosticsResponse, Status>> + Send>>;

    async fn follow_backend_diagnostics(
        &self,
        request: Request<FollowBackendDiagnosticsRequest>,
    ) -> Result<Response<Self::FollowBackendDiagnosticsStream>, Status> {
        let request = request.into_inner();
        let mut resume = unique_positions(request.resume_positions)?;
        let declared = self.machines.declared()?;
        let selected = select_machines(declared, request.machine_ids)?;

        let cancellation = CancellationToken::new();
        let (sender, receiver) = mpsc::channel(FOLLOW_BUFFER);
        let mut initial = Vec::new();
        for selection in selected {
            let machine = match selection {
                Selection::Declared(machine) => machine,
                Selection::Undeclared(machine_id) => {
                    initial.push(state(
                        &machine_id,
                        &machine_id,
                        DiagnosticSourceState::Ended,
                    ));
                    continue;
                }
            };
            let Some(permit) = self.acquire_follow(&machine.id) else {
                record_unavailable(&machine.id, "follow", "capacity_exhausted");
                initial.push(state(
                    &machine.id,
                    &machine.display_name,
                    DiagnosticSourceState::Unavailable,
                ));
                continue;
            };
            let resume = resume.remove(&machine.id);
            tokio::spawn(follow_machine(
                Arc::clone(&self.machines),
                machine,
                resume,
                sender.clone(),
                cancellation.clone(),
                permit,
            ));
        }
        drop(sender);

        let events = tokio_stream::iter(initial)
            .chain(tokio_stream::wrappers::ReceiverStream::new(receiver))
            .map(Ok);
        Ok(Response::new(Box::pin(CancellableDiagnosticStream::new(
            Box::pin(events),
            cancellation,
        ))))
    }
}

/// Per-machine access to each SiteNode's retained diagnostics, as the manager reaches them.
pub(crate) trait MachineDiagnostics: Send + Sync {
    /// Lists the SiteNodes currently declared in the site, in identity order.
    fn declared(&self) -> Result<Vec<DeclaredMachine>, Status>;

    /// Reads one bounded page of `machine_id`; `None` when the machine is unavailable.
    fn read<'a>(
        &'a self,
        machine_id: &'a str,
        page_size: u32,
        before: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Option<NodeDiagnosticHistory>>;

    /// Opens a follow of `machine_id` whose first frame was not a rejection; `None` when the
    /// machine is unavailable. Dropping the frames stops the machine's read.
    fn follow<'a>(
        &'a self,
        machine_id: &'a str,
        resume: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Option<MachineFrames>>;
}

/// Ordered frames of one admitted machine follow, ending when its call ends.
pub(crate) type MachineFrames = Pin<Box<dyn Stream<Item = Frame> + Send>>;

/// A SiteNode a diagnostic request can read, with its operator-facing name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeclaredMachine {
    pub(crate) id: String,
    pub(crate) display_name: String,
}

/// The manager's reader of SiteNode diagnostics: the node sharing the manager process in process,
/// every other node over its pinned private diagnostics channel.
#[derive(Clone)]
pub(crate) struct NodeDiagnostics {
    /// Channels to other nodes; absent on a manager without a private listener.
    channels: Option<WorkerChannels>,
    site: SiteRepository,
    coordinator: ManagerNodeControlCoordinator,
    local: Option<(String, NodeDiagnosticReader)>,
    deadline: Duration,
}

impl NodeDiagnostics {
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
            deadline: NODE_LOG_DEADLINE,
        }
    }

    /// Reads `node_id`, the SiteNode sharing the manager process, through its own `reader`.
    pub(crate) fn with_local_node(mut self, node_id: String, reader: NodeDiagnosticReader) -> Self {
        self.local = Some((node_id, reader));
        self
    }

    /// Returns the connection to `node_id`, or records why it is unreachable.
    fn connect(&self, node_id: &str) -> Option<NodeConnection> {
        if let Some((_, reader)) = self.local.as_ref().filter(|(local, _)| local == node_id) {
            let endpoint = self.coordinator.node_endpoint(node_id).filter(|endpoint| {
                endpoint.connection_state != CoordinatorConnectionState::Disconnected
            });
            let Some(endpoint) = endpoint else {
                record_unavailable(node_id, "connect", "not_connected");
                return None;
            };
            return Some(NodeConnection {
                transport: NodeTransport::InProcess(reader.clone()),
                manager_session: self.coordinator.manager_session().clone(),
                node_incarnation: endpoint.incarnation,
            });
        }
        let Some(channels) = &self.channels else {
            record_unavailable(node_id, "connect", "private_transport_unconfigured");
            return None;
        };
        channels
            .connect(&self.site, &self.coordinator, node_id)
            .map(NodeConnection::from)
            .inspect_err(|error| {
                debug!(
                    event_kind = "node_diagnostic_unavailable",
                    node_id,
                    reason = "channel_unavailable",
                    error = %error,
                    "Manager could not reach a node for its diagnostics"
                );
            })
            .ok()
    }
}

impl MachineDiagnostics for NodeDiagnostics {
    fn declared(&self) -> Result<Vec<DeclaredMachine>, Status> {
        let site = self
            .site
            .snapshot()
            .map_err(|_| Status::unavailable("Site state is unavailable; retry shortly"))?;
        let mut machines = site
            .nodes
            .into_iter()
            .map(|node| DeclaredMachine {
                id: node.node_id,
                display_name: node.display_name,
            })
            .collect::<Vec<_>>();
        machines.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(machines)
    }

    fn read<'a>(
        &'a self,
        machine_id: &'a str,
        page_size: u32,
        before: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Option<NodeDiagnosticHistory>> {
        Box::pin(async move {
            let connection = self.connect(machine_id)?;
            let request = ReadNodeDiagnosticsRequest {
                target: Some(connection.target()),
                page_size,
                before_position: before,
            };
            #[expect(
                clippy::disallowed_methods,
                reason = "bounds a real call to a node's diagnostics service"
            )]
            let response = match tokio::time::timeout(self.deadline, connection.read(request)).await
            {
                Ok(Ok(response)) => response,
                Ok(Err(status)) => {
                    record_unavailable(machine_id, "read_history", status_reason(&status));
                    return None;
                }
                Err(_) => {
                    record_unavailable(machine_id, "read_history", "deadline_exceeded");
                    return None;
                }
            };
            match response.outcome {
                Some(read_node_diagnostics_response::Outcome::History(history)) => Some(history),
                Some(read_node_diagnostics_response::Outcome::Rejected(rejected)) => {
                    record_unavailable(machine_id, "read_history", rejection_name(rejected.reason));
                    None
                }
                None => {
                    record_unavailable(machine_id, "read_history", "protocol_violation");
                    None
                }
            }
        })
    }

    fn follow<'a>(
        &'a self,
        machine_id: &'a str,
        resume: Option<Vec<u8>>,
    ) -> BoxFuture<'a, Option<MachineFrames>> {
        Box::pin(async move {
            let connection = self.connect(machine_id)?;
            let request = FollowNodeDiagnosticsRequest {
                target: Some(connection.target()),
                resume_position: resume,
            };
            let opening = async {
                let mut frames = connection.follow(request).await?;
                let first = frames.next().await.transpose()?;
                Ok::<_, Status>((frames, first))
            };
            #[expect(
                clippy::disallowed_methods,
                reason = "bounds a real call to a node's diagnostics service"
            )]
            let (frames, first) = match tokio::time::timeout(self.deadline, opening).await {
                Ok(Ok(opened)) => opened,
                Ok(Err(status)) => {
                    record_unavailable(machine_id, "follow", status_reason(&status));
                    return None;
                }
                Err(_) => {
                    record_unavailable(machine_id, "follow", "deadline_exceeded");
                    return None;
                }
            };
            let first = match first.and_then(|first| first.frame) {
                Some(Frame::Rejected(rejected)) => {
                    record_unavailable(machine_id, "follow", rejection_name(rejected.reason));
                    return None;
                }
                Some(frame) => frame,
                None => {
                    record_unavailable(machine_id, "follow", "protocol_violation");
                    return None;
                }
            };
            // A transport failure ends the frames; the caller reads that as an unavailable machine.
            let rest = frames
                .take_while(|frame| std::future::ready(frame.is_ok()))
                .filter_map(|frame| std::future::ready(frame.ok().and_then(|frame| frame.frame)));
            let frames: MachineFrames = Box::pin(tokio_stream::once(first).chain(rest));
            Some(frames)
        })
    }
}

/// One reachable node with the identities a diagnostic request to it must name.
struct NodeConnection {
    transport: NodeTransport,
    manager_session: ManagerSession,
    node_incarnation: NodeIncarnation,
}

/// How one node's diagnostic service is reached.
enum NodeTransport {
    /// Another node, over its pinned private diagnostics channel.
    Remote(Channel),

    /// The node sharing the manager process, through its own reader.
    InProcess(NodeDiagnosticReader),
}

impl From<WorkerConnection> for NodeConnection {
    fn from(connection: WorkerConnection) -> Self {
        Self {
            transport: NodeTransport::Remote(connection.channel),
            manager_session: connection.manager_session,
            node_incarnation: connection.node_incarnation,
        }
    }
}

impl NodeConnection {
    fn target(&self) -> NodeDiagnosticTarget {
        NodeDiagnosticTarget {
            manager_session_id: self.manager_session.as_str().to_owned(),
            node_incarnation_id: self.node_incarnation.as_str().to_owned(),
        }
    }

    async fn read(
        self,
        request: ReadNodeDiagnosticsRequest,
    ) -> Result<dotcore_proto::dotcore::diagnostic::private::v1::ReadNodeDiagnosticsResponse, Status>
    {
        match self.transport {
            NodeTransport::Remote(channel) => client(channel)
                .read_node_diagnostics(request)
                .await
                .map(Response::into_inner),
            NodeTransport::InProcess(reader) => Ok(reader.read(request).await),
        }
    }

    async fn follow(
        self,
        request: FollowNodeDiagnosticsRequest,
    ) -> Result<NodeFollowFrames, Status> {
        match self.transport {
            NodeTransport::Remote(channel) => {
                let frames = client(channel)
                    .follow_node_diagnostics(request)
                    .await?
                    .into_inner();
                Ok(Box::pin(frames))
            }
            NodeTransport::InProcess(reader) => Ok(reader.follow(request)),
        }
    }
}

/// One machine's part of a site history read.
struct MachineHistory {
    machine: DeclaredMachine,

    /// The machine's page; `None` when it was unavailable.
    history: Option<NodeDiagnosticHistory>,
}

/// Merges per-machine pages into the newest `page_size` events within the response limit.
///
/// A machine keeps its own continuation: the node's own when every returned event was kept, the
/// oldest kept event when the merge dropped older ones, or a position re-reading the whole page
/// when the merge dropped all of them. An unavailable machine has no continuation, so its older
/// history is not paged again until the next newest read.
fn merge_histories(
    page_size: usize,
    machines: Vec<MachineHistory>,
    newest: bool,
) -> ReadBackendDiagnosticsResponse {
    let mut events = machines
        .iter()
        .enumerate()
        .flat_map(|(index, machine)| {
            machine
                .history
                .iter()
                .flat_map(|history| history.events.iter())
                .map(move |event| (index, event))
        })
        .collect::<Vec<_>>();
    // Stable, so each machine's events keep the node's own order.
    events.sort_by(|(left_index, left), (right_index, right)| {
        timestamp_key(left)
            .cmp(&timestamp_key(right))
            .then_with(|| {
                machines[*left_index]
                    .machine
                    .id
                    .cmp(&machines[*right_index].machine.id)
            })
    });

    let event_bytes = events
        .iter()
        .map(|(index, event)| {
            let event = MachineDiagnosticEvent {
                machine_id: machines[*index].machine.id.clone(),
                event: Some((*event).clone()),
            };
            1 + prost::length_delimiter_len(event.encoded_len()) + event.encoded_len()
        })
        .collect::<Vec<_>>();
    let mut start = events.len().saturating_sub(page_size);
    let mut merge_truncated = false;
    let mut metadata = merge_metadata(&machines, &events[start..], newest);
    while start < events.len()
        && event_bytes[start..].iter().sum::<usize>() + metadata.encoded_len() > MAX_RESPONSE_BYTES
    {
        start += 1;
        merge_truncated = true;
        metadata = merge_metadata(&machines, &events[start..], newest);
    }

    metadata.events = events[start..]
        .iter()
        .map(|(index, event)| MachineDiagnosticEvent {
            machine_id: machines[*index].machine.id.clone(),
            event: Some((*event).clone()),
        })
        .collect();
    metadata.response_truncated |= merge_truncated;
    metadata
}

/// Builds the merged response's continuation and machine states, without events, for the
/// events kept in `kept`.
fn merge_metadata(
    machines: &[MachineHistory],
    kept: &[(usize, &BackendDiagnosticEvent)],
    newest: bool,
) -> ReadBackendDiagnosticsResponse {
    let mut positions = Vec::new();
    let mut states = Vec::with_capacity(machines.len());
    let mut older = false;
    let mut truncated = false;
    for (index, machine) in machines.iter().enumerate() {
        let id = machine.machine.id.clone();
        // An unavailable machine leaves the continuation: keeping its cursor would let an older
        // page that reads nothing else repeat forever.
        let Some(history) = &machine.history else {
            states.push(state_message(
                &id,
                &machine.machine.display_name,
                DiagnosticSourceState::Unavailable,
                None,
            ));
            continue;
        };
        let oldest_kept = kept.iter().find(|(kept, _)| *kept == index);
        let kept_count = kept.iter().filter(|(kept, _)| *kept == index).count();
        let dropped = kept_count < history.events.len();
        let next = if !dropped {
            history.next_before_position.clone()
        } else if let Some((_, event)) = oldest_kept {
            Some(event.position.clone())
        } else {
            history.newest_inclusive_position.clone()
        };
        if let Some(position) = next {
            positions.push(MachineDiagnosticPosition {
                machine_id: id.clone(),
                position,
            });
        }
        older |=
            dropped || history.history_state == DiagnosticHistoryState::OlderOutputMayRemain as i32;
        truncated |= history.response_truncated;
        states.push(MachineDiagnosticState {
            machine_id: id,
            display_name: machine.machine.display_name.clone(),
            state: history.source_state,
            latest_position: if newest {
                history.latest_position.clone()
            } else {
                None
            },
        });
    }
    ReadBackendDiagnosticsResponse {
        events: Vec::new(),
        next_before: (!positions.is_empty()).then_some(DiagnosticHistoryContinuation { positions }),
        machines: states,
        history_state: if older {
            DiagnosticHistoryState::OlderOutputMayRemain
        } else {
            DiagnosticHistoryState::NoOlderOutputObserved
        } as i32,
        response_truncated: truncated,
    }
}

/// Forwards one machine's follow until it becomes unavailable, its call ends, or the caller
/// leaves. Unavailability is final for this follow: the caller reconnects the machine itself.
async fn follow_machine(
    machines: Arc<dyn MachineDiagnostics>,
    machine: DeclaredMachine,
    resume: Option<Vec<u8>>,
    sender: mpsc::Sender<FollowBackendDiagnosticsResponse>,
    cancellation: CancellationToken,
    _permit: OwnedSemaphorePermit,
) {
    let unavailable = state(
        &machine.id,
        &machine.display_name,
        DiagnosticSourceState::Unavailable,
    );
    let frames = tokio::select! {
        () = cancellation.cancelled() => return,
        frames = machines.follow(&machine.id, resume) => frames,
    };
    let Some(mut frames) = frames else {
        let _ = sender.send(unavailable).await;
        return;
    };
    loop {
        let frame = tokio::select! {
            () = cancellation.cancelled() => return,
            frame = frames.next() => frame,
        };
        let (response, last) = match frame {
            Some(Frame::Diagnostic(event)) => (
                FollowBackendDiagnosticsResponse {
                    event: Some(Event::Diagnostic(MachineDiagnosticEvent {
                        machine_id: machine.id.clone(),
                        event: Some(event),
                    })),
                },
                false,
            ),
            Some(Frame::SourceState(source_state)) => (
                state_with(&machine.id, &machine.display_name, source_state),
                source_state == DiagnosticSourceState::Unavailable as i32,
            ),
            Some(Frame::Rejected(_)) | None => {
                record_unavailable(&machine.id, "follow", "call_ended");
                (unavailable.clone(), true)
            }
        };
        let sent = tokio::select! {
            () = cancellation.cancelled() => return,
            sent = sender.send(response) => sent.is_ok(),
        };
        if !sent || last {
            return;
        }
    }
}

/// How one requested follow machine resolves against the declared SiteNodes.
enum Selection {
    Declared(DeclaredMachine),
    Undeclared(String),
}

/// Resolves requested machines in request order; an empty request selects every declared one.
fn select_machines(
    declared: Vec<DeclaredMachine>,
    requested: Vec<String>,
) -> Result<Vec<Selection>, Status> {
    if requested.is_empty() {
        return Ok(declared.into_iter().map(Selection::Declared).collect());
    }
    let mut declared = declared
        .into_iter()
        .map(|machine| (machine.id.clone(), machine))
        .collect::<BTreeMap<_, _>>();
    let mut selections = Vec::with_capacity(requested.len());
    for (index, machine_id) in requested.iter().enumerate() {
        if requested[..index].contains(machine_id) {
            return Err(Status::invalid_argument(
                "Diagnostic machine identities must be unique",
            ));
        }
        selections.push(match declared.remove(machine_id) {
            Some(machine) => Selection::Declared(machine),
            None => Selection::Undeclared(machine_id.clone()),
        });
    }
    Ok(selections)
}

fn unique_positions(
    positions: Vec<MachineDiagnosticPosition>,
) -> Result<BTreeMap<String, Vec<u8>>, Status> {
    let mut unique = BTreeMap::new();
    for position in positions {
        if unique
            .insert(position.machine_id, position.position)
            .is_some()
        {
            return Err(Status::invalid_argument(
                "Diagnostic positions must name each machine at most once",
            ));
        }
    }
    Ok(unique)
}

fn timestamp_key(event: &BackendDiagnosticEvent) -> (i64, i32) {
    event
        .timestamp
        .as_ref()
        .map_or((0, 0), |timestamp| (timestamp.seconds, timestamp.nanos))
}

fn state(
    machine_id: &str,
    display_name: &str,
    source_state: DiagnosticSourceState,
) -> FollowBackendDiagnosticsResponse {
    state_with(machine_id, display_name, source_state as i32)
}

fn state_with(
    machine_id: &str,
    display_name: &str,
    source_state: i32,
) -> FollowBackendDiagnosticsResponse {
    FollowBackendDiagnosticsResponse {
        event: Some(Event::SourceEvent(MachineDiagnosticState {
            machine_id: machine_id.to_owned(),
            display_name: display_name.to_owned(),
            state: source_state,
            latest_position: None,
        })),
    }
}

fn state_message(
    machine_id: &str,
    display_name: &str,
    source_state: DiagnosticSourceState,
    latest_position: Option<Vec<u8>>,
) -> MachineDiagnosticState {
    MachineDiagnosticState {
        machine_id: machine_id.to_owned(),
        display_name: display_name.to_owned(),
        state: source_state as i32,
        latest_position,
    }
}

fn record_unavailable(node_id: &str, operation: &'static str, reason: &str) {
    debug!(
        event_kind = "node_diagnostic_unavailable",
        operation, node_id, reason, "Node diagnostics are unavailable"
    );
}

fn rejection_name(reason: i32) -> &'static str {
    dotcore_proto::dotcore::diagnostic::private::v1::NodeDiagnosticRejectionReason::try_from(reason)
        .map_or("unknown_rejection", |reason| reason.as_str_name())
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

fn capacity_exhausted() -> Status {
    Status::resource_exhausted("Diagnostic request capacity is exhausted; retry shortly")
}

pub(super) fn client(channel: Channel) -> NodeDiagnosticServiceClient<Channel> {
    NodeDiagnosticServiceClient::new(channel)
        .max_decoding_message_size(MESSAGE_LIMIT_BYTES)
        .max_encoding_message_size(MESSAGE_LIMIT_BYTES)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, HashMap},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use dotcore_proto::{
        dotcore::diagnostic::{
            private::v1::{NodeDiagnosticHistory, follow_node_diagnostics_response::Frame},
            v1::{
                BackendDiagnosticEvent, DiagnosticHistoryContinuation, DiagnosticHistoryState,
                DiagnosticSourceState, FollowBackendDiagnosticsRequest,
                FollowBackendDiagnosticsResponse, MachineDiagnosticPosition,
                ReadBackendDiagnosticsRequest, ReadBackendDiagnosticsResponse,
                diagnostic_service_server::DiagnosticService as _,
                follow_backend_diagnostics_response::Event,
            },
        },
        google::protobuf::Timestamp,
    };
    use futures_util::{StreamExt, future::BoxFuture};
    use tokio::sync::mpsc;
    use tonic::{Request, Status};

    use super::{
        DeclaredMachine, MAX_HISTORY_READS, MAX_MACHINE_FOLLOWS, MachineDiagnostics, MachineFrames,
        SiteDiagnosticService,
    };

    #[tokio::test]
    async fn history_pages_interleaved_machines_without_skips_or_duplicates() {
        // Machine a logs every second, machine b every third second, so pages split unevenly.
        let machines = FakeMachines::new()
            .with_events("a", (0..25).map(|second| second * 2).collect())
            .with_events("b", (0..10).map(|second| second * 3 + 1).collect());
        let service = SiteDiagnosticService::new(machines.clone());

        let mut seen = Vec::new();
        let mut before = None;
        let mut pages = 0;
        loop {
            let page = read(&service, 7, before.take()).await;
            pages += 1;
            let page_events = page
                .events
                .iter()
                .map(|event| (event.machine_id.clone(), seconds(event.event.as_ref())))
                .collect::<Vec<_>>();
            assert!(page_events.len() <= 7);
            assert!(
                page_events.windows(2).all(|pair| pair[0].1 <= pair[1].1),
                "page {pages} is chronological: {page_events:?}"
            );
            if let Some((_, oldest_seen)) = seen.first() {
                assert!(
                    page_events.last().is_none_or(|last| last.1 <= *oldest_seen),
                    "page {pages} is older than the previous page"
                );
            }
            seen.splice(0..0, page_events);
            match page.next_before {
                Some(next) => before = Some(next),
                None => break,
            }
            assert!(pages < 20, "paging terminates");
        }

        let expected = (0..25)
            .map(|second| ("a".to_owned(), second * 2))
            .chain((0..10).map(|second| ("b".to_owned(), second * 3 + 1)))
            .collect::<BTreeSet<_>>();
        assert_eq!(seen.len(), expected.len(), "no event repeats");
        assert_eq!(seen.into_iter().collect::<BTreeSet<_>>(), expected);
    }

    #[tokio::test]
    async fn newest_page_reports_machine_states_and_follow_handoff_positions() {
        let machines = FakeMachines::new()
            .with_events("a", vec![1, 2])
            .with_unavailable("b");
        let service = SiteDiagnosticService::new(machines);

        let page = read(&service, 10, None).await;

        assert_eq!(page.machines.len(), 2);
        assert_eq!(page.machines[0].machine_id, "a");
        assert_eq!(page.machines[0].display_name, "Machine a");
        assert_eq!(
            page.machines[0].state,
            DiagnosticSourceState::Available as i32
        );
        assert_eq!(page.machines[0].latest_position, Some(position(2)));
        assert_eq!(page.machines[1].machine_id, "b");
        assert_eq!(
            page.machines[1].state,
            DiagnosticSourceState::Unavailable as i32
        );
        assert_eq!(page.machines[1].latest_position, None);
        assert_eq!(
            page.history_state,
            DiagnosticHistoryState::NoOlderOutputObserved as i32
        );
        assert_eq!(
            page.next_before, None,
            "b was never read and a has no older page"
        );
    }

    #[tokio::test]
    async fn a_machine_whose_whole_page_was_dropped_rereads_it_on_the_next_page() {
        let machines = FakeMachines::new()
            .with_events("a", vec![1, 2])
            .with_events("b", vec![10, 11]);
        let service = SiteDiagnosticService::new(machines);

        let newest = read(&service, 2, None).await;
        assert_eq!(machine_seconds(&newest), [("b", 10), ("b", 11)]);
        let older = read(&service, 2, newest.next_before).await;

        assert_eq!(machine_seconds(&older), [("a", 1), ("a", 2)]);
        assert_eq!(older.next_before, None);
    }

    #[tokio::test]
    async fn an_empty_page_with_a_source_cursor_still_advances() {
        let cursor = b"source:older".to_vec();
        let machines = FakeMachines::new().with_history(
            "a",
            NodeDiagnosticHistory {
                next_before_position: Some(cursor.clone()),
                history_state: DiagnosticHistoryState::OlderOutputMayRemain as i32,
                source_state: DiagnosticSourceState::Truncated as i32,
                response_truncated: true,
                ..Default::default()
            },
        );
        let service = SiteDiagnosticService::new(machines);

        let page = read(&service, 10, None).await;

        assert!(page.events.is_empty());
        assert!(page.response_truncated);
        assert_eq!(
            page.history_state,
            DiagnosticHistoryState::OlderOutputMayRemain as i32
        );
        assert_eq!(page.next_before, Some(continuation(&[("a", cursor)])));
    }

    #[tokio::test]
    async fn older_pages_skip_unlisted_machines_and_drop_an_unavailable_one() {
        let machines = FakeMachines::new()
            .with_events("a", vec![1, 2, 3])
            .with_unavailable("b")
            .with_events("c", vec![1]);
        let service = SiteDiagnosticService::new(machines.clone());

        let page = read(
            &service,
            10,
            Some(continuation(&[
                ("a", position(3)),
                ("b", position(9)),
                ("gone", position(1)),
            ])),
        )
        .await;

        assert_eq!(machine_seconds(&page), [("a", 1), ("a", 2)]);
        assert_eq!(
            page.next_before, None,
            "an exhausted page cannot be requested again"
        );
        assert_eq!(
            page.history_state,
            DiagnosticHistoryState::NoOlderOutputObserved as i32
        );
        assert_eq!(
            page.machines[1].state,
            DiagnosticSourceState::Unavailable as i32
        );
        assert!(
            page.machines
                .iter()
                .all(|machine| machine.latest_position.is_none())
        );
        assert_eq!(
            machines.reads(),
            ["a", "b"],
            "c and undeclared machines are not read"
        );
    }

    #[tokio::test]
    async fn merged_pages_stay_within_the_response_limit() {
        let large = "m".repeat(60 * 1024);
        let machines = FakeMachines::new()
            .with_large_events("a", 80, &large)
            .with_large_events("b", 80, &large);
        let service = SiteDiagnosticService::new(machines);

        let page = read(&service, 200, None).await;

        assert!(page.response_truncated);
        assert!(prost::Message::encoded_len(&page) <= super::MAX_RESPONSE_BYTES);
        let next = page.next_before.expect("dropped events remain pageable");
        assert_eq!(next.positions.len(), 2);
    }

    #[tokio::test]
    async fn history_reads_are_bounded() {
        let machines = FakeMachines::new().with_events("a", vec![1]);
        let gate = machines.gate_reads();
        let service = SiteDiagnosticService::new(machines);

        let pending = (0..MAX_HISTORY_READS)
            .map(|_| {
                let service = service.clone();
                tokio::spawn(async move { read(&service, 10, None).await })
            })
            .collect::<Vec<_>>();
        tokio::time::timeout(Duration::from_secs(5), async {
            while service.histories.available_permits() > 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every read holds its permit");

        let refused = service
            .read_backend_diagnostics(Request::new(ReadBackendDiagnosticsRequest::default()))
            .await
            .expect_err("a read over the limit is refused");
        assert_eq!(refused.code(), tonic::Code::ResourceExhausted);

        gate.add_permits(MAX_HISTORY_READS);
        for read in pending {
            assert_eq!(read.await.expect("read").events.len(), 1);
        }
    }

    #[tokio::test]
    async fn follow_reports_an_unavailable_machine_once_while_others_continue() {
        let machines = FakeMachines::new()
            .with_follow("a")
            .with_follow("b")
            .with_unavailable("c");
        let service = SiteDiagnosticService::new(machines.clone());
        let mut stream = follow(&service, &[]).await;

        let a = machines.follower("a").await;
        let b = machines.follower("b").await;
        let mut first = vec![next(&mut stream).await];
        a.send(Frame::SourceState(DiagnosticSourceState::Available as i32))
            .await
            .expect("a frame");
        first.push(next(&mut stream).await);
        assert!(first.contains(&source_state("c", DiagnosticSourceState::Unavailable)));
        assert!(first.contains(&source_state("a", DiagnosticSourceState::Available)));

        b.send(Frame::SourceState(
            DiagnosticSourceState::Unavailable as i32,
        ))
        .await
        .expect("b frame");
        assert_eq!(
            next(&mut stream).await,
            source_state("b", DiagnosticSourceState::Unavailable)
        );
        tokio::time::timeout(Duration::from_secs(5), b.closed())
            .await
            .expect("an unavailable machine is no longer followed");

        a.send(Frame::Diagnostic(event(5))).await.expect("a event");
        let Event::Diagnostic(received) = next(&mut stream).await else {
            panic!("expected a's event");
        };
        assert_eq!(received.machine_id, "a");
        assert_eq!(seconds(received.event.as_ref()), 5);

        drop(a);
        assert_eq!(
            next(&mut stream).await,
            source_state("a", DiagnosticSourceState::Unavailable),
            "a lost call reads as unavailable"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("stream ends")
                .is_none(),
            "the stream ends once no machine is followed"
        );
    }

    #[tokio::test]
    async fn narrowed_follow_resumes_one_machine_and_ends_an_undeclared_one() {
        let machines = FakeMachines::new().with_follow("a").with_follow("b");
        let service = SiteDiagnosticService::new(machines.clone());

        let request = FollowBackendDiagnosticsRequest {
            machine_ids: vec!["gone".to_owned(), "b".to_owned()],
            resume_positions: vec![MachineDiagnosticPosition {
                machine_id: "b".to_owned(),
                position: position(4),
            }],
        };
        let mut stream = service
            .follow_backend_diagnostics(Request::new(request))
            .await
            .expect("follow")
            .into_inner();

        assert_eq!(
            next_ok(&mut stream).await,
            FollowBackendDiagnosticsResponse {
                event: Some(Event::SourceEvent(
                    dotcore_proto::dotcore::diagnostic::v1::MachineDiagnosticState {
                        machine_id: "gone".to_owned(),
                        display_name: "gone".to_owned(),
                        state: DiagnosticSourceState::Ended as i32,
                        latest_position: None,
                    }
                )),
            }
            .event
            .expect("event")
        );
        machines.follower("b").await;
        assert_eq!(machines.follows(), [("b".to_owned(), Some(position(4)))]);
    }

    #[tokio::test]
    async fn follows_of_one_machine_are_bounded_and_released_on_drop() {
        let machines = FakeMachines::new().with_follow("a").with_follow("b");
        let service = SiteDiagnosticService::new(machines.clone());
        let mut streams = Vec::new();
        for _ in 0..MAX_MACHINE_FOLLOWS {
            streams.push(follow(&service, &["a"]).await);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while machines.follows().len() < MAX_MACHINE_FOLLOWS {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("every follow reaches the machine");

        let mut over = follow(&service, &["a", "b"]).await;
        assert_eq!(
            next(&mut over).await,
            source_state("a", DiagnosticSourceState::Unavailable),
            "the machine over its limit is unavailable"
        );
        assert!(
            service.acquire_follow("a").is_none(),
            "held follows keep their permits"
        );
        drop(over);

        drop(streams.pop());
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(permit) = service.acquire_follow("a") {
                    drop(permit);
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("a dropped follow releases its permit");
    }

    #[tokio::test]
    async fn duplicate_machines_or_positions_are_invalid() {
        let service = SiteDiagnosticService::new(FakeMachines::new().with_follow("a"));

        let duplicate_machine = service
            .follow_backend_diagnostics(Request::new(FollowBackendDiagnosticsRequest {
                machine_ids: vec!["a".to_owned(), "a".to_owned()],
                resume_positions: Vec::new(),
            }))
            .await
            .err()
            .expect("duplicate machines are refused");
        let duplicate_position = service
            .read_backend_diagnostics(Request::new(ReadBackendDiagnosticsRequest {
                page_size: 0,
                before: Some(continuation(&[("a", position(1)), ("a", position(2))])),
            }))
            .await
            .expect_err("duplicate positions are refused");

        assert_eq!(duplicate_machine.code(), tonic::Code::InvalidArgument);
        assert_eq!(duplicate_position.code(), tonic::Code::InvalidArgument);
    }

    /// Scripted machines: in-memory paged histories, unavailable machines, and follows fed by
    /// the test through channels.
    #[derive(Clone, Default)]
    struct FakeMachines {
        machines: Arc<Mutex<Vec<(String, FakeMachine)>>>,
        reads: Arc<Mutex<Vec<String>>>,
        follows: Arc<Mutex<Vec<FollowCall>>>,
        followers: Arc<Mutex<HashMap<String, Vec<mpsc::Sender<Frame>>>>>,
        read_gate: Arc<Mutex<Option<Arc<tokio::sync::Semaphore>>>>,
    }

    /// One follow the manager opened: the machine and its resume position.
    type FollowCall = (String, Option<Vec<u8>>);

    #[derive(Clone)]
    enum FakeMachine {
        /// Events at the given seconds, paged like a node: newest first, exclusive positions.
        Events(Vec<BackendDiagnosticEvent>),
        History(NodeDiagnosticHistory),
        Follow,
        Unavailable,
    }

    impl FakeMachines {
        fn new() -> Self {
            Self::default()
        }

        fn with(self, id: &str, machine: FakeMachine) -> Self {
            self.machines
                .lock()
                .expect("machines")
                .push((id.to_owned(), machine));
            self
        }

        fn with_events(self, id: &str, seconds: Vec<i64>) -> Self {
            self.with(
                id,
                FakeMachine::Events(seconds.into_iter().map(event).collect()),
            )
        }

        fn with_large_events(self, id: &str, count: i64, message: &str) -> Self {
            let events = (0..count)
                .map(|second| BackendDiagnosticEvent {
                    message: message.to_owned(),
                    ..event(second)
                })
                .collect();
            self.with(id, FakeMachine::Events(events))
        }

        fn with_history(self, id: &str, history: NodeDiagnosticHistory) -> Self {
            self.with(id, FakeMachine::History(history))
        }

        fn with_follow(self, id: &str) -> Self {
            self.with(id, FakeMachine::Follow)
        }

        fn with_unavailable(self, id: &str) -> Self {
            self.with(id, FakeMachine::Unavailable)
        }

        /// Holds every read until the returned semaphore gains permits.
        fn gate_reads(&self) -> Arc<tokio::sync::Semaphore> {
            let gate = Arc::new(tokio::sync::Semaphore::new(0));
            *self.read_gate.lock().expect("gate") = Some(Arc::clone(&gate));
            gate
        }

        fn machine(&self, id: &str) -> Option<FakeMachine> {
            self.machines
                .lock()
                .expect("machines")
                .iter()
                .find(|(machine, _)| machine == id)
                .map(|(_, machine)| machine.clone())
        }

        fn reads(&self) -> Vec<String> {
            let mut reads = self.reads.lock().expect("reads").clone();
            reads.sort();
            reads
        }

        fn follows(&self) -> Vec<FollowCall> {
            self.follows.lock().expect("follows").clone()
        }

        /// Waits until the manager follows `id`, then returns the sender feeding its frames.
        async fn follower(&self, id: &str) -> mpsc::Sender<Frame> {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let sender = self
                        .followers
                        .lock()
                        .expect("followers")
                        .get_mut(id)
                        .and_then(Vec::pop);
                    if let Some(sender) = sender {
                        return sender;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("machine followed in time")
        }
    }

    impl MachineDiagnostics for FakeMachines {
        fn declared(&self) -> Result<Vec<DeclaredMachine>, Status> {
            let mut machines = self
                .machines
                .lock()
                .expect("machines")
                .iter()
                .map(|(id, _)| DeclaredMachine {
                    id: id.clone(),
                    display_name: format!("Machine {id}"),
                })
                .collect::<Vec<_>>();
            machines.sort_by(|left, right| left.id.cmp(&right.id));
            Ok(machines)
        }

        fn read<'a>(
            &'a self,
            machine_id: &'a str,
            page_size: u32,
            before: Option<Vec<u8>>,
        ) -> BoxFuture<'a, Option<NodeDiagnosticHistory>> {
            Box::pin(async move {
                self.reads
                    .lock()
                    .expect("reads")
                    .push(machine_id.to_owned());
                let gate = self.read_gate.lock().expect("gate").clone();
                if let Some(gate) = gate {
                    gate.acquire().await.expect("gate").forget();
                }
                match self.machine(machine_id)? {
                    FakeMachine::Events(events) => Some(page(&events, page_size, before)),
                    FakeMachine::History(history) => Some(history),
                    FakeMachine::Follow | FakeMachine::Unavailable => None,
                }
            })
        }

        fn follow<'a>(
            &'a self,
            machine_id: &'a str,
            resume: Option<Vec<u8>>,
        ) -> BoxFuture<'a, Option<MachineFrames>> {
            Box::pin(async move {
                self.follows
                    .lock()
                    .expect("follows")
                    .push((machine_id.to_owned(), resume));
                let FakeMachine::Follow = self.machine(machine_id)? else {
                    return None;
                };
                let (sender, receiver) = mpsc::channel(16);
                self.followers
                    .lock()
                    .expect("followers")
                    .entry(machine_id.to_owned())
                    .or_default()
                    .push(sender);
                let frames: MachineFrames =
                    Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver));
                Some(frames)
            })
        }
    }

    /// Pages like a node: positions are event indexes plus one, so a position is exclusive and
    /// the newest inclusive position is one past the newest returned event.
    fn page(
        events: &[BackendDiagnosticEvent],
        page_size: u32,
        before: Option<Vec<u8>>,
    ) -> NodeDiagnosticHistory {
        let end = before.map_or(events.len(), |before| index(&before));
        let start = end.saturating_sub(page_size as usize);
        let page = events[start..end]
            .iter()
            .enumerate()
            .map(|(offset, event)| BackendDiagnosticEvent {
                position: position_index(start + offset),
                ..event.clone()
            })
            .collect::<Vec<_>>();
        NodeDiagnosticHistory {
            next_before_position: (start > 0).then(|| position_index(start)),
            latest_position: page.last().map(|event| event.position.clone()),
            newest_inclusive_position: (!page.is_empty()).then(|| position_index(end)),
            history_state: if start > 0 {
                DiagnosticHistoryState::OlderOutputMayRemain
            } else {
                DiagnosticHistoryState::NoOlderOutputObserved
            } as i32,
            source_state: DiagnosticSourceState::Available as i32,
            response_truncated: false,
            events: page,
        }
    }

    /// Position naming event index `index`: exclusive for that event and everything newer.
    fn position_index(index: usize) -> Vec<u8> {
        index.to_string().into_bytes()
    }

    fn index(position: &[u8]) -> usize {
        std::str::from_utf8(position)
            .expect("position")
            .parse()
            .expect("index position")
    }

    /// The fake's position of the event at `second` in a machine whose events are 1, 2, 3...
    fn position(second: usize) -> Vec<u8> {
        position_index(second - 1)
    }

    fn event(second: i64) -> BackendDiagnosticEvent {
        BackendDiagnosticEvent {
            timestamp: Some(Timestamp {
                seconds: second,
                nanos: 0,
            }),
            level: "INFO".to_owned(),
            target: "test".to_owned(),
            message: format!("at {second}"),
            ..Default::default()
        }
    }

    fn seconds(event: Option<&BackendDiagnosticEvent>) -> i64 {
        event
            .and_then(|event| event.timestamp.as_ref())
            .map(|timestamp| timestamp.seconds)
            .expect("event timestamp")
    }

    fn machine_seconds(page: &ReadBackendDiagnosticsResponse) -> Vec<(&str, i64)> {
        page.events
            .iter()
            .map(|event| (event.machine_id.as_str(), seconds(event.event.as_ref())))
            .collect()
    }

    fn continuation(positions: &[(&str, Vec<u8>)]) -> DiagnosticHistoryContinuation {
        DiagnosticHistoryContinuation {
            positions: positions
                .iter()
                .map(|(machine_id, position)| MachineDiagnosticPosition {
                    machine_id: (*machine_id).to_owned(),
                    position: position.clone(),
                })
                .collect(),
        }
    }

    fn source_state(machine_id: &str, state: DiagnosticSourceState) -> Event {
        Event::SourceEvent(
            dotcore_proto::dotcore::diagnostic::v1::MachineDiagnosticState {
                machine_id: machine_id.to_owned(),
                display_name: format!("Machine {machine_id}"),
                state: state as i32,
                latest_position: None,
            },
        )
    }

    async fn read(
        service: &SiteDiagnosticService,
        page_size: u32,
        before: Option<DiagnosticHistoryContinuation>,
    ) -> ReadBackendDiagnosticsResponse {
        service
            .read_backend_diagnostics(Request::new(ReadBackendDiagnosticsRequest {
                page_size,
                before,
            }))
            .await
            .expect("history page")
            .into_inner()
    }

    type FollowStream = <SiteDiagnosticService as dotcore_proto::dotcore::diagnostic::v1::diagnostic_service_server::DiagnosticService>::FollowBackendDiagnosticsStream;

    async fn follow(service: &SiteDiagnosticService, machine_ids: &[&str]) -> FollowStream {
        service
            .follow_backend_diagnostics(Request::new(FollowBackendDiagnosticsRequest {
                machine_ids: machine_ids.iter().map(|id| (*id).to_owned()).collect(),
                resume_positions: Vec::new(),
            }))
            .await
            .expect("follow")
            .into_inner()
    }

    async fn next(stream: &mut FollowStream) -> Event {
        next_ok(stream).await
    }

    async fn next_ok(stream: &mut FollowStream) -> Event {
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .expect("event in time")
            .expect("event")
            .expect("event without error")
            .event
            .expect("event payload")
    }
}
