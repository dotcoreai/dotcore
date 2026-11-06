//! Serves one node's retained backend logs to the authenticated site manager, in process for the
//! node sharing the manager process or on a worker's private listener.
//!
//! Each request names the manager session and the node incarnation; the node answers only the
//! manager, for its own incarnation and the session whose view it installed, and follows at most
//! a fixed number of streams at once. Everything after the caller check is independent of the
//! transport carrying the request.

use std::{pin::Pin, sync::Arc};

use dotcore_proto::dotcore::diagnostic::private::v1::{
    FollowNodeDiagnosticsRequest, FollowNodeDiagnosticsResponse, NodeDiagnosticRejection,
    NodeDiagnosticRejectionReason, NodeDiagnosticTarget, ReadNodeDiagnosticsRequest,
    ReadNodeDiagnosticsResponse, follow_node_diagnostics_response,
    node_diagnostic_service_server::{
        NodeDiagnosticService as NodeDiagnosticServiceContract, NodeDiagnosticServiceServer,
    },
    read_node_diagnostics_response,
};
use dotcore_site::{
    private_transport::{AuthenticatedRoleAuthorization, PrivateNodeRole, PrivateServiceContext},
    view::fencing::{AcceptedNodeViewAuthority, ServingAuthorizationError},
};
use futures_util::StreamExt;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::debug;

use super::{
    MESSAGE_LIMIT_BYTES, resolve_page_size,
    retained::{CancellableDiagnosticStream, RetainedLogs},
};

/// Concurrent follows one node serves. It exceeds the manager's per-machine follow limit, so a
/// reconnecting follow is never refused while its predecessor's call is still winding down.
const MAX_NODE_FOLLOWS: usize = 16;

/// Ordered frames of one node follow, whatever transport carries them.
pub(super) type NodeFollowFrames =
    Pin<Box<dyn tokio_stream::Stream<Item = Result<FollowNodeDiagnosticsResponse, Status>> + Send>>;

/// Node-hosted private receiver for manager diagnostic reads.
pub(crate) struct NodeDiagnosticService {
    reader: NodeDiagnosticReader,
    authorization: AuthenticatedRoleAuthorization,
}

impl NodeDiagnosticService {
    /// Creates a receiver authorized for the manager that answers through `reader`.
    pub(crate) fn new(reader: NodeDiagnosticReader, context: &PrivateServiceContext) -> Self {
        Self {
            reader,
            authorization: context.authorize_role(PrivateNodeRole::Manager),
        }
    }

    /// Wraps the receiver in its gRPC server with limits sized for one full history page.
    pub(crate) fn into_server(self) -> NodeDiagnosticServiceServer<Self> {
        NodeDiagnosticServiceServer::new(self)
            .max_decoding_message_size(MESSAGE_LIMIT_BYTES)
            .max_encoding_message_size(MESSAGE_LIMIT_BYTES)
    }

    /// Checks that this node serves diagnostics and that the caller is the manager.
    fn authorize<T>(&self, request: &Request<T>) -> Result<(), Status> {
        if self.authorization.receiver_role() != PrivateNodeRole::Worker {
            return Err(Status::permission_denied(
                "node diagnostics are served only by nodes",
            ));
        }
        self.authorization.authorize(request)?;
        Ok(())
    }
}

#[tonic::async_trait]
impl NodeDiagnosticServiceContract for NodeDiagnosticService {
    type FollowNodeDiagnosticsStream = NodeFollowFrames;

    async fn read_node_diagnostics(
        &self,
        request: Request<ReadNodeDiagnosticsRequest>,
    ) -> Result<Response<ReadNodeDiagnosticsResponse>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(self.reader.read(request.into_inner()).await))
    }

    async fn follow_node_diagnostics(
        &self,
        request: Request<FollowNodeDiagnosticsRequest>,
    ) -> Result<Response<Self::FollowNodeDiagnosticsStream>, Status> {
        self.authorize(&request)?;
        Ok(Response::new(self.reader.follow(request.into_inner())))
    }
}

/// Node-local retained-log reads for the manager, whatever transport carries its request.
///
/// Each request is fenced by manager session and node incarnation; follows hold one of a fixed
/// number of admission permits until dropped.
#[derive(Clone)]
pub(crate) struct NodeDiagnosticReader {
    retained: RetainedLogs,
    view: AcceptedNodeViewAuthority,
    follows: Arc<Semaphore>,
}

impl NodeDiagnosticReader {
    /// Creates a reader of the retained logs under the node's Site or node folder `root`.
    pub(crate) fn new(root: &std::path::Path, view: AcceptedNodeViewAuthority) -> Self {
        Self {
            retained: RetainedLogs::new(root),
            view,
            follows: Arc::new(Semaphore::new(MAX_NODE_FOLLOWS)),
        }
    }

    /// Reads one bounded history page, or says why it was refused.
    pub(super) async fn read(
        &self,
        request: ReadNodeDiagnosticsRequest,
    ) -> ReadNodeDiagnosticsResponse {
        let outcome = match self.read_admitted(request).await {
            Ok(history) => read_node_diagnostics_response::Outcome::History(history),
            Err(reason) => {
                record_rejection("read_history", reason);
                read_node_diagnostics_response::Outcome::Rejected(rejection(reason))
            }
        };
        ReadNodeDiagnosticsResponse {
            outcome: Some(outcome),
        }
    }

    /// Opens a follow of the active retained file; a refusal is its single frame.
    pub(super) fn follow(&self, request: FollowNodeDiagnosticsRequest) -> NodeFollowFrames {
        match self.follow_admitted(request) {
            Ok(frames) => frames,
            Err(reason) => {
                record_rejection("follow", reason);
                let frame = FollowNodeDiagnosticsResponse {
                    frame: Some(follow_node_diagnostics_response::Frame::Rejected(
                        rejection(reason),
                    )),
                };
                Box::pin(tokio_stream::once(Ok(frame)))
            }
        }
    }

    async fn read_admitted(
        &self,
        request: ReadNodeDiagnosticsRequest,
    ) -> Result<
        dotcore_proto::dotcore::diagnostic::private::v1::NodeDiagnosticHistory,
        NodeDiagnosticRejectionReason,
    > {
        self.authorize(request.target.as_ref())?;
        let page_size = resolve_page_size(request.page_size)
            .ok_or(NodeDiagnosticRejectionReason::InvalidRequest)?;
        self.retained
            .read(page_size, request.before_position.as_deref())
            .await
            .map_err(|_| NodeDiagnosticRejectionReason::InvalidRequest)
    }

    fn follow_admitted(
        &self,
        request: FollowNodeDiagnosticsRequest,
    ) -> Result<NodeFollowFrames, NodeDiagnosticRejectionReason> {
        self.authorize(request.target.as_ref())?;
        // Admit before following: the follow starts its reader task immediately.
        let permit = self.acquire()?;
        let cancellation = CancellationToken::new();
        let events = self
            .retained
            .follow(request.resume_position.as_deref(), cancellation.clone())
            .map_err(|_| NodeDiagnosticRejectionReason::InvalidRequest)?;
        let frames = events.map(move |frame| {
            let _permit = &permit;
            Ok(frame)
        });
        Ok(Box::pin(CancellableDiagnosticStream::new(
            frames,
            cancellation,
        )))
    }

    /// Checks the target names this node incarnation and the manager session whose view the node
    /// installed.
    fn authorize(
        &self,
        target: Option<&NodeDiagnosticTarget>,
    ) -> Result<(), NodeDiagnosticRejectionReason> {
        let target = target.ok_or(NodeDiagnosticRejectionReason::InvalidRequest)?;
        self.view
            .authorize_manager_request(&target.manager_session_id, &target.node_incarnation_id)
            .map_err(|error| match error {
                ServingAuthorizationError::NodeIncarnationMismatch => {
                    NodeDiagnosticRejectionReason::NodeIncarnationMismatch
                }
                ServingAuthorizationError::ManagerSessionMismatch
                | ServingAuthorizationError::DeploymentNotAssigned => {
                    NodeDiagnosticRejectionReason::ManagerSessionMismatch
                }
            })
    }

    fn acquire(&self) -> Result<OwnedSemaphorePermit, NodeDiagnosticRejectionReason> {
        Arc::clone(&self.follows)
            .try_acquire_owned()
            .map_err(|_| NodeDiagnosticRejectionReason::CapacityExhausted)
    }
}

fn rejection(reason: NodeDiagnosticRejectionReason) -> NodeDiagnosticRejection {
    NodeDiagnosticRejection {
        reason: reason as i32,
    }
}

fn record_rejection(operation: &'static str, reason: NodeDiagnosticRejectionReason) {
    debug!(
        event_kind = "node_diagnostic_rejected",
        operation,
        reason = reason.as_str_name(),
        "Node refused a manager diagnostic request"
    );
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, time::Duration};

    use dotcore_proto::dotcore::diagnostic::{
        private::v1::{
            FollowNodeDiagnosticsRequest, NodeDiagnosticRejectionReason, NodeDiagnosticTarget,
            ReadNodeDiagnosticsRequest, follow_node_diagnostics_response::Frame,
            node_diagnostic_service_server::NodeDiagnosticService as _,
            read_node_diagnostics_response::Outcome,
        },
        v1::DiagnosticSourceState,
    };
    use dotcore_site::{
        private_transport::{AuthenticatedMachineIdentity, PrivateNodeRole, PrivateServiceContext},
        view::fencing::{
            AcceptedNodeViewAuthority, ManagerSession, NodeIncarnation, NodeViewProjection,
        },
        worker_channels::{WorkerChannelPurpose, WorkerChannels},
    };
    use futures_util::StreamExt;
    use tonic::Request;

    use super::{MAX_NODE_FOLLOWS, NodeDiagnosticReader, NodeDiagnosticService};
    use crate::{
        server::private::PrivateServiceAssembly, test_support::private_tls::TestAuthority,
    };

    const SITE_ID: &str = "site-one";
    const NODE_ID: &str = "worker-one";
    const MANAGER_ID: &str = "manager-one";

    #[tokio::test]
    async fn only_the_manager_is_admitted_by_a_worker() {
        let logs = Logs::new(&["first"]);
        let service = NodeDiagnosticService::new(
            logs.reader(),
            &PrivateServiceContext::new(PrivateNodeRole::Worker, NODE_ID),
        );

        let unauthenticated = service
            .read_node_diagnostics(authenticated(read(target()), None))
            .await
            .expect_err("an unauthenticated caller is refused");
        let from_worker = service
            .read_node_diagnostics(authenticated(read(target()), Some(PrivateNodeRole::Worker)))
            .await
            .expect_err("another worker is refused");
        let from_manager = service
            .read_node_diagnostics(authenticated(
                read(target()),
                Some(PrivateNodeRole::Manager),
            ))
            .await
            .expect("the manager is admitted")
            .into_inner();

        assert_eq!(unauthenticated.code(), tonic::Code::Unauthenticated);
        assert_eq!(from_worker.code(), tonic::Code::PermissionDenied);
        let Some(Outcome::History(history)) = from_manager.outcome else {
            panic!("expected history");
        };
        assert_eq!(messages(&history.events), ["first"]);
    }

    #[tokio::test]
    async fn read_returns_positioned_events_and_pages_older() {
        let logs = Logs::new(&["first", "second", "third"]);
        let reader = logs.reader();

        let newest = history(reader.read(page(target(), 2, None)).await);
        assert_eq!(messages(&newest.events), ["second", "third"]);
        assert!(newest.events.iter().all(|event| !event.position.is_empty()));
        assert_eq!(
            newest.latest_position.as_ref(),
            Some(&newest.events[1].position)
        );

        let older = history(
            reader
                .read(page(target(), 2, newest.next_before_position.clone()))
                .await,
        );
        assert_eq!(messages(&older.events), ["first"]);

        let again = history(
            reader
                .read(page(target(), 2, newest.newest_inclusive_position.clone()))
                .await,
        );
        assert_eq!(messages(&again.events), ["second", "third"]);
    }

    #[tokio::test]
    async fn read_rejects_stale_identities_and_invalid_requests() {
        let logs = Logs::new(&["first"]);
        let reader = logs.reader();
        let cases = [
            (
                NodeDiagnosticTarget {
                    node_incarnation_id: "2".repeat(32),
                    ..target()
                },
                100,
                None,
                NodeDiagnosticRejectionReason::NodeIncarnationMismatch,
            ),
            (
                NodeDiagnosticTarget {
                    manager_session_id: "b".repeat(32),
                    ..target()
                },
                100,
                None,
                NodeDiagnosticRejectionReason::ManagerSessionMismatch,
            ),
            (
                target(),
                201,
                None,
                NodeDiagnosticRejectionReason::InvalidRequest,
            ),
            (
                target(),
                100,
                Some(b"0:../../etc/passwd:1".to_vec()),
                NodeDiagnosticRejectionReason::InvalidRequest,
            ),
        ];

        for (target, page_size, before, expected) in cases {
            let response = reader.read(page(target, page_size, before)).await;
            let Some(Outcome::Rejected(rejected)) = response.outcome else {
                panic!("expected {expected:?}");
            };
            assert_eq!(rejected.reason, expected as i32);
        }
        let missing_target = reader.read(ReadNodeDiagnosticsRequest::default()).await;
        assert!(matches!(
            missing_target.outcome,
            Some(Outcome::Rejected(rejected))
                if rejected.reason == NodeDiagnosticRejectionReason::InvalidRequest as i32
        ));
    }

    #[tokio::test]
    async fn follow_streams_new_events_and_bounds_concurrent_follows() {
        let logs = Logs::new(&[]);
        let reader = logs.reader();
        let mut follows = Vec::new();
        for _ in 0..MAX_NODE_FOLLOWS {
            let mut frames = reader.follow(follow(target()));
            assert_eq!(
                next(&mut frames).await,
                Frame::SourceState(DiagnosticSourceState::Available as i32)
            );
            follows.push(frames);
        }

        let mut refused = reader.follow(follow(target()));
        assert!(matches!(
            next(&mut refused).await,
            Frame::Rejected(rejected)
                if rejected.reason == NodeDiagnosticRejectionReason::CapacityExhausted as i32
        ));
        assert!(
            refused.next().await.is_none(),
            "a refusal is the only frame"
        );

        drop(follows.pop());
        let mut admitted = reader.follow(follow(target()));
        assert_eq!(
            next(&mut admitted).await,
            Frame::SourceState(DiagnosticSourceState::Available as i32)
        );
        logs.append("appended");
        let Frame::Diagnostic(event) = next(&mut admitted).await else {
            panic!("expected the appended event");
        };
        assert_eq!(event.message, "appended");
    }

    #[tokio::test]
    async fn follow_rejects_a_stale_manager_session() {
        let logs = Logs::new(&[]);
        let mut frames = logs.reader().follow(follow(NodeDiagnosticTarget {
            manager_session_id: "b".repeat(32),
            ..target()
        }));

        assert!(matches!(
            next(&mut frames).await,
            Frame::Rejected(rejected)
                if rejected.reason == NodeDiagnosticRejectionReason::ManagerSessionMismatch as i32
        ));
        assert!(frames.next().await.is_none());
    }

    #[tokio::test]
    async fn a_full_history_page_crosses_the_private_channel() {
        let large = "m".repeat(60 * 1024);
        let logs = Logs::new(&vec![large.as_str(); 100]);
        let authority = TestAuthority::new();
        let (worker_certificate, worker_key) = authority.issue(SITE_ID, NODE_ID, true);
        let manager = authority.issue(SITE_ID, MANAGER_ID, false);
        let reader = logs.reader();
        let server = crate::server::private::prepare(
            authority.private_configuration(
                SITE_ID,
                NODE_ID,
                PrivateNodeRole::Worker,
                (worker_certificate.clone(), worker_key),
                [(MANAGER_ID, PrivateNodeRole::Manager, manager.0.as_slice())],
            ),
            &dotcore_domain::timing::Timing::production(),
            PrivateServiceAssembly::new(move |context, routes| {
                routes.add_service(NodeDiagnosticService::new(reader, &context).into_server());
            })
            .assemble(PrivateNodeRole::Worker, NODE_ID),
            std::future::pending(),
        )
        .await
        .expect("worker private listener");
        let address = server.address;
        tokio::spawn(server.serve());
        let channels = WorkerChannels::new(&authority.private_configuration(
            SITE_ID,
            MANAGER_ID,
            PrivateNodeRole::Manager,
            manager,
            [(
                NODE_ID,
                PrivateNodeRole::Worker,
                worker_certificate.as_slice(),
            )],
        ));
        let channel = channels
            .channel(NODE_ID, WorkerChannelPurpose::Diagnostics, address.into())
            .expect("worker channel");

        let response = crate::diagnostic::site::client(channel)
            .read_node_diagnostics(page(target(), 200, None))
            .await
            .expect("a full page decodes")
            .into_inner();

        let page = history(response);
        assert!(
            page.response_truncated,
            "the node filled its response limit"
        );
        assert!(page.events.len() > 60, "{} events", page.events.len());
        assert!(
            prost::Message::encoded_len(&page) > crate::diagnostic::MAX_RESPONSE_BYTES - 128 * 1024
        );
    }

    /// A node root whose active retained file holds one record per message.
    struct Logs {
        root: PathBuf,
    }

    impl Logs {
        fn new(messages: &[&str]) -> Self {
            let root = dotcore_domain::test_support::canonical_temp_dir().join(format!(
                "dotcore-node-diagnostic-{}",
                dotcore_domain::test_support::unique_suffix()
            ));
            fs::create_dir_all(root.join("logs")).expect("log directory");
            let logs = Self { root };
            fs::write(logs.active(), "").expect("active log");
            for message in messages {
                logs.append(message);
            }
            logs
        }

        fn reader(&self) -> NodeDiagnosticReader {
            NodeDiagnosticReader::new(&self.root, installed_view())
        }

        fn append(&self, message: &str) {
            use std::io::Write;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(self.active())
                .expect("active log");
            writeln!(
                file,
                "{{\"timestamp\":\"2026-09-15T01:00:00Z\",\"level\":\"INFO\",\"target\":\"test\",\"fields\":{{\"message\":\"{message}\"}}}}"
            )
            .expect("append record");
        }

        fn active(&self) -> PathBuf {
            self.root.join("logs/backend.2026-09-15.jsonl")
        }
    }

    impl Drop for Logs {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn installed_view() -> AcceptedNodeViewAuthority {
        let view = AcceptedNodeViewAuthority::new(
            SITE_ID,
            NODE_ID,
            NodeIncarnation::parse("1".repeat(32)).expect("node incarnation"),
            ManagerSession::parse("a".repeat(32)).expect("manager session"),
            Duration::from_secs(30),
        )
        .expect("view authority");
        let projection = NodeViewProjection::new(Vec::new(), Vec::new()).expect("projection");
        view.install_for_test(SITE_ID, NODE_ID, 1, &projection)
            .expect("installed view");
        view
    }

    fn target() -> NodeDiagnosticTarget {
        NodeDiagnosticTarget {
            manager_session_id: "a".repeat(32),
            node_incarnation_id: "1".repeat(32),
        }
    }

    fn read(target: NodeDiagnosticTarget) -> ReadNodeDiagnosticsRequest {
        page(target, 0, None)
    }

    fn page(
        target: NodeDiagnosticTarget,
        page_size: u32,
        before_position: Option<Vec<u8>>,
    ) -> ReadNodeDiagnosticsRequest {
        ReadNodeDiagnosticsRequest {
            target: Some(target),
            page_size,
            before_position,
        }
    }

    fn follow(target: NodeDiagnosticTarget) -> FollowNodeDiagnosticsRequest {
        FollowNodeDiagnosticsRequest {
            target: Some(target),
            resume_position: None,
        }
    }

    fn history(
        response: dotcore_proto::dotcore::diagnostic::private::v1::ReadNodeDiagnosticsResponse,
    ) -> dotcore_proto::dotcore::diagnostic::private::v1::NodeDiagnosticHistory {
        match response.outcome {
            Some(Outcome::History(history)) => history,
            other => panic!("expected history, got {other:?}"),
        }
    }

    fn messages(
        events: &[dotcore_proto::dotcore::diagnostic::v1::BackendDiagnosticEvent],
    ) -> Vec<&str> {
        events.iter().map(|event| event.message.as_str()).collect()
    }

    async fn next(frames: &mut super::NodeFollowFrames) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), frames.next())
            .await
            .expect("frame in time")
            .expect("frame")
            .expect("frame without transport error")
            .frame
            .expect("frame payload")
    }

    fn authenticated<T>(message: T, role: Option<PrivateNodeRole>) -> Request<T> {
        let mut request = Request::new(message);
        if let Some(role) = role {
            request
                .extensions_mut()
                .insert(AuthenticatedMachineIdentity {
                    site_id: SITE_ID.to_owned(),
                    node_id: MANAGER_ID.to_owned(),
                    role,
                });
        }
        request
    }
}
