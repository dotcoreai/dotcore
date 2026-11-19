//! Serves the authenticated inter-node boundary on its own listener.
//!
//! TLS establishes site trust, and request ingress checks the peer's complete leaf against the
//! live pins and binds its exact SiteNode identity before any private service receives a request.
//! The one exception is the join service: while a join is active, a machine with no site
//! certificate yet may call it, and nothing else. The site's private transport owns the identity
//! core, the pins and connection acceptance this listener uses.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use dotcore_domain::timing::{PrivateTiming, Timing};
use dotcore_proto::dotcore::node::v1::node_join_service_server;
use dotcore_site::private_transport::{
    DEFAULT_MAX_UNPINNED_CONNECTIONS, JoinGate, PeerPins, PrivateConnectionInfo,
    PrivateConnections, PrivateNodeRole, PrivateServerConfiguration, PrivateServiceContext,
    SiteMembership, UnpinnedConnectionLimits, authentication_failure, exact_node_identity,
    not_a_member,
};
use futures_util::future::{self, Either, Ready};
use tonic::Status;
use tower::{Layer, Service};

type PrivateServiceAssembler =
    dyn FnOnce(PrivateServiceContext, &mut tonic::service::RoutesBuilder) + Send;

/// Deferred private-service assembly invoked after local node custody is established.
pub struct PrivateServiceAssembly {
    assemble: Box<PrivateServiceAssembler>,
}

impl PrivateServiceAssembly {
    /// Creates an assembly that adds receiver-local services to a shared route builder.
    pub fn new(
        assemble: impl FnOnce(PrivateServiceContext, &mut tonic::service::RoutesBuilder)
        + Send
        + 'static,
    ) -> Self {
        Self {
            assemble: Box::new(assemble),
        }
    }

    /// Combines independent component assemblies for one private listener.
    pub(crate) fn combine(self, other: Self) -> Self {
        Self::new(move |context, routes| {
            (self.assemble)(context.clone(), routes);
            (other.assemble)(context, routes);
        })
    }

    pub(crate) fn empty() -> Self {
        Self::new(|_, _| {})
    }

    /// Assembles every service for the receiving SiteNode `receiver_node_id` acting as
    /// `receiver_role`.
    pub(crate) fn assemble(
        self,
        receiver_role: PrivateNodeRole,
        receiver_node_id: &str,
    ) -> tonic::service::Routes {
        let mut routes = tonic::service::Routes::builder();
        let context = PrivateServiceContext::new(receiver_role, receiver_node_id);
        (self.assemble)(context, &mut routes);
        routes.routes()
    }
}

pub(crate) struct PreparedPrivateServer {
    pub address: SocketAddr,
    future: Pin<Box<dyn Future<Output = Result<(), super::ServerError>> + Send>>,
}

impl PreparedPrivateServer {
    pub async fn serve(self) -> Result<(), super::ServerError> {
        self.future.await
    }
}

/// What a server's private listener serves: its mTLS identity, its routes, the timing of its
/// connections, what it admits from connections without a pinned certificate, and, on the site
/// manager, the site membership that tells a removed node so.
pub(crate) struct PrivateListener {
    pub(crate) configuration: PrivateServerConfiguration,
    pub(crate) routes: tonic::service::Routes,
    pub(crate) timing: PrivateTiming,
    pub(crate) unpinned: UnpinnedConnectionLimits,
    pub(crate) join_gate: JoinGate,
    pub(crate) membership: Option<Arc<dyn SiteMembership>>,
}

impl PrivateListener {
    /// Describes a listener with no join active and the default bounds on connections without a
    /// pinned certificate.
    pub(crate) fn new(
        configuration: PrivateServerConfiguration,
        routes: tonic::service::Routes,
        timing: &Timing,
    ) -> Self {
        Self {
            configuration,
            routes,
            timing: timing.private,
            unpinned: UnpinnedConnectionLimits {
                max_open: DEFAULT_MAX_UNPINNED_CONNECTIONS,
                idle_timeout: timing.join.idle_timeout,
            },
            join_gate: JoinGate::default(),
            membership: None,
        }
    }
}

/// Binds and prepares the private listener with exact ingress authentication. It pings every
/// connection at the keepalive interval of its timing, so a long-lived stream from a peer that
/// vanished without closing its connection ends.
pub(crate) async fn prepare(
    listener: PrivateListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<PreparedPrivateServer, super::ServerError> {
    let PrivateListener {
        configuration,
        routes,
        timing,
        unpinned,
        join_gate,
        membership,
    } = listener;
    let tcp = tokio::net::TcpListener::bind(configuration.bind_address)
        .await
        .map_err(|source| super::ListenerError::Private {
            address: configuration.bind_address,
            source,
        })?;
    let address = tcp
        .local_addr()
        .map_err(super::ListenerError::PrivateAddress)?;
    let connections =
        PrivateConnections::new(&configuration, unpinned).map_err(super::ServerError::Private)?;
    let ingress = PrivateIngress::new(&configuration, join_gate, membership);
    let future = tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(timing.keepalive_interval))
        .http2_keepalive_timeout(Some(timing.keepalive_timeout))
        .layer(ingress)
        .add_routes(routes)
        .serve_with_incoming_shutdown(connections.incoming(tcp), shutdown);
    Ok(PreparedPrivateServer {
        address,
        future: Box::pin(async move {
            future
                .await
                .map_err(|source| super::ServerError::Private(anyhow::Error::new(source)))
        }),
    })
}

/// Request ingress of the private listener: a request to the join service passes while a join
/// is active, and every other request must come from a pinned site certificate, whose exact
/// identity it then carries to the private services. A site certificate of a node the site no
/// longer declares is told it isn't a member.
#[derive(Clone)]
struct PrivateIngress {
    site_id: Arc<str>,
    pins: PeerPins,
    join_gate: JoinGate,
    membership: Option<Arc<dyn SiteMembership>>,
}

impl PrivateIngress {
    fn new(
        configuration: &PrivateServerConfiguration,
        join_gate: JoinGate,
        membership: Option<Arc<dyn SiteMembership>>,
    ) -> Self {
        Self {
            site_id: Arc::from(configuration.site_id.as_str()),
            pins: configuration.peers.clone(),
            join_gate,
            membership,
        }
    }

    /// Admits one request, attaching the authenticated identity of a pinned peer. Every refusal
    /// is the same authentication failure, so it reveals neither the pins nor whether a join is
    /// active, except for a certificate the site's authority issued to a node the site no longer
    /// declares, which is told so. The TLS handshake already checked that certificate's chain.
    fn admit<B>(&self, request: &mut http::Request<B>) -> Result<(), Status> {
        if is_join_request(request) {
            return self
                .join_gate
                .is_open()
                .then_some(())
                .ok_or_else(authentication_failure);
        }
        let leaf = request
            .extensions()
            .get::<PrivateConnectionInfo>()
            .and_then(PrivateConnectionInfo::peer_certificate);
        let Some(identity) = leaf.and_then(|leaf| self.pins.authenticate(&self.site_id, leaf))
        else {
            return Err(self.refusal(leaf));
        };
        request.extensions_mut().insert(identity);
        Ok(())
    }

    /// Tells a removed node it isn't a member, and refuses anything else as an authentication
    /// failure, including a declared node whose pin is missing or differs.
    fn refusal(&self, leaf: Option<&[u8]>) -> Status {
        let removed = self.membership.as_ref().is_some_and(|membership| {
            leaf.and_then(|leaf| exact_node_identity(leaf, &self.site_id))
                .is_some_and(|node_id| !membership.declares(&node_id))
        });
        if removed {
            not_a_member()
        } else {
            authentication_failure()
        }
    }
}

impl<S> Layer<S> for PrivateIngress {
    type Service = PrivateIngressService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PrivateIngressService {
            ingress: self.clone(),
            inner,
        }
    }
}

/// The private routes behind [`PrivateIngress`].
#[derive(Clone)]
struct PrivateIngressService<S> {
    ingress: PrivateIngress,
    inner: S,
}

impl<S, B> Service<http::Request<B>> for PrivateIngressService<S>
where
    S: Service<http::Request<B>, Response = http::Response<tonic::body::Body>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = Either<Ready<Result<Self::Response, Self::Error>>, S::Future>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: http::Request<B>) -> Self::Future {
        match self.ingress.admit(&mut request) {
            Ok(()) => Either::Right(self.inner.call(request)),
            Err(status) => Either::Left(future::ready(Ok(status.into_http()))),
        }
    }
}

/// Reports whether a request calls the join service, the one service a machine without a site
/// certificate may reach.
fn is_join_request<B>(request: &http::Request<B>) -> bool {
    request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|path| path.split_once('/'))
        .is_some_and(|(service, _)| service == node_join_service_server::SERVICE_NAME)
}

#[cfg(test)]
mod tests {
    use std::{
        net::SocketAddr,
        sync::{Arc, Mutex},
    };

    use dotcore_domain::timing::Timing;
    use dotcore_proto::dotcore::{
        node::v1::{
            CommitRequest, CommitResponse, JoinRequest, JoinResponse,
            node_join_service_client::NodeJoinServiceClient,
            node_join_service_server::{NodeJoinService, NodeJoinServiceServer},
        },
        site::private::v1::{
            StreamNodeControlRequest, StreamNodeControlResponse,
            node_control_service_client::NodeControlServiceClient,
            node_control_service_server::{NodeControlService, NodeControlServiceServer},
        },
    };
    use dotcore_site::{
        node::folder::{CertificateFingerprint, certificate_identity_uri},
        private_transport::{
            JoinGate, PeerPins, PinnedPeerVerifier, PrivateConnectionInfo, PrivatePeerPolicy,
            SiteMembership, UnpinnedConnectionLimits, is_not_a_member, pem,
        },
        test_support::private_tls::TestAuthority as SiteAuthority,
        worker_channels::{WorkerChannelError, WorkerChannelPurpose, WorkerChannels},
    };
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose, SanType,
    };
    use rustls::{
        client::danger::ServerCertVerifier as _,
        pki_types::{CertificateDer, ServerName, UnixTime},
    };
    use time::{Duration, OffsetDateTime};
    use tonic::{
        Code, Request, Response, Status,
        transport::{Channel, ClientTlsConfig, Endpoint, Identity},
    };

    use super::{PrivateListener, PrivateNodeRole, prepare};

    const SITE_ID: &str = "site-one";
    const MANAGER_ID: &str = "manager-node";
    const WORKER_ID: &str = "worker-one";

    /// Deadline for an outcome that arrives at once on loopback.
    const DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

    #[tokio::test]
    async fn certificate_less_requests_reach_only_the_join_service_while_a_join_is_active() {
        let site = TestSite::new();
        let joins = FakeJoinHost::default();
        let gate = JoinGate::default();
        let manager = site
            .serve_manager(PeerPins::default(), gate.clone(), Some(joins.clone()))
            .await;
        let channel = site.manager_client(manager, None).await;

        let closed = join(&channel).await.expect_err("no join is active");
        assert_authentication_failure(&closed);
        assert_eq!(joins.connections(), Vec::<(u64, bool)>::new());

        let open = gate.open();
        join(&channel)
            .await
            .expect("an active join admits the request");
        let control = node_control(&channel)
            .await
            .expect_err("node control needs a pinned certificate");
        assert_authentication_failure(&control);
        let connection = joins.connections();
        assert_eq!(connection.len(), 1);
        assert!(
            !connection[0].1,
            "the joining machine presented no certificate"
        );

        drop(open);
        let ended = join(&channel).await.expect_err("the join ended");
        assert_authentication_failure(&ended);
        assert_eq!(joins.connections(), connection);
    }

    #[tokio::test]
    async fn certificate_less_http2_clients_with_keepalive_are_idle_closed() {
        let site = TestSite::new();
        let joins = FakeJoinHost::default();
        let gate = JoinGate::default();
        let _open = gate.open();
        let idle_timeout = std::time::Duration::from_millis(300);
        let manager = site
            .serve_manager_with(gate, joins.clone(), 1, idle_timeout)
            .await;
        // Pings far more often than the idle timeout keep bytes flowing both ways.
        let channel = site
            .client(manager, MANAGER_ID, &site.manager.0, None)
            .http2_keep_alive_interval(idle_timeout / 6)
            .keep_alive_while_idle(true)
            .connect()
            .await
            .expect("manager channel");

        join(&channel)
            .await
            .expect("an active join admits the request");
        let [connection] = joins.infos().try_into().expect("one join connection");
        tokio::time::timeout(DEADLINE, connection.closed())
            .await
            .expect("pings don't keep a connection without a join request open");
    }

    #[tokio::test]
    async fn a_new_certificate_less_connection_replaces_one_that_presented_no_secret() {
        let site = TestSite::new();
        let joins = FakeJoinHost::default();
        let gate = JoinGate::default();
        let _open = gate.open();
        let manager = site
            .serve_manager_with(gate, joins.clone(), 1, DEADLINE * 6)
            .await;
        let held = site.manager_client(manager, None).await;
        join(&held).await.expect("the first connection is served");
        let [first] = joins.infos().try_into().expect("one join connection");

        let joining = site.manager_client(manager, None).await;
        join(&joining)
            .await
            .expect("a new connection is served at the limit");
        tokio::time::timeout(DEADLINE, first.closed())
            .await
            .expect("the held connection gives way");
        let infos = joins.infos();
        assert_eq!(infos.len(), 2);
        assert_ne!(infos[1].id(), first.id());
    }

    #[tokio::test]
    async fn a_worker_listener_refuses_connections_without_a_certificate() {
        let site = TestSite::new();
        let worker = site.serve_worker().await;

        let without = match site
            .client(worker, WORKER_ID, &site.worker.0, None)
            .connect()
            .await
        {
            Ok(channel) => node_control(&channel).await.expect_err("no certificate"),
            Err(error) => Status::unavailable(error.to_string()),
        };
        assert_ne!(without.code(), Code::Unauthenticated, "{without:?}");
        assert_ne!(without.code(), Code::Unimplemented, "{without:?}");

        let manager = site.identity(&site.manager);
        let with = site
            .client(worker, WORKER_ID, &site.worker.0, Some(&manager))
            .connect()
            .await
            .expect("the manager connects");
        let reached = node_control(&with)
            .await
            .expect_err("no node control service on the worker");
        assert_eq!(reached.code(), Code::Unimplemented, "{reached:?}");
    }

    #[tokio::test]
    async fn a_pin_added_at_runtime_authenticates_on_ingress_and_in_worker_channels() {
        let site = TestSite::new();
        let pins = PeerPins::default();
        let manager = site
            .serve_manager(pins.clone(), JoinGate::default(), None)
            .await;
        let worker = site.serve_worker().await;
        let channels = WorkerChannels::new(
            &site.manager_configuration(pins.clone()),
            Timing::test().private,
        );
        let from_worker = site.manager_client(manager, Some(&site.worker)).await;

        let refused = node_control(&from_worker)
            .await
            .expect_err("the worker is not pinned yet");
        assert_authentication_failure(&refused);
        assert!(matches!(
            channels.channel(WORKER_ID, WorkerChannelPurpose::Inference, worker.into()),
            Err(WorkerChannelError::MissingPin)
        ));

        pins.insert(
            WORKER_ID,
            PrivatePeerPolicy {
                role: PrivateNodeRole::Worker,
                certificate_sha256: CertificateFingerprint::for_certificate(&site.worker.0),
            },
        );

        // Past ingress, the manager serves no node control here, so the request is unknown.
        let admitted = node_control(&from_worker)
            .await
            .expect_err("no node control service");
        assert_eq!(admitted.code(), Code::Unimplemented, "{admitted:?}");
        let to_worker = channels
            .channel(WORKER_ID, WorkerChannelPurpose::Inference, worker.into())
            .expect("the new pin builds a worker channel");
        let reached = node_control(&to_worker)
            .await
            .expect_err("no node control service on the worker");
        assert_eq!(reached.code(), Code::Unimplemented, "{reached:?}");
    }

    #[tokio::test]
    async fn removing_a_pin_closes_its_open_streams_and_tells_a_removed_node_it_is_not_a_member() {
        let site = TestSite::new();
        let pins = PeerPins::default();
        pins.insert(WORKER_ID, site.worker_pin());
        let membership = Arc::new(FakeMembership::declaring([WORKER_ID]));
        let manager = site
            .serve_manager_with_membership(pins.clone(), Arc::clone(&membership))
            .await;
        let from_worker = site.manager_client(manager, Some(&site.worker)).await;
        let mut stream = NodeControlServiceClient::new(from_worker.clone())
            .stream_node_control(futures_util::stream::pending())
            .await
            .expect("a pinned worker opens its stream")
            .into_inner();

        // A declared node whose pin is gone gets the generic answer, not a removal.
        pins.remove(WORKER_ID);
        let ended = tokio::time::timeout(DEADLINE, stream.message())
            .await
            .expect("the open stream ends once its pin is withdrawn");
        assert!(!matches!(ended, Ok(Some(_))), "{ended:?}");
        let refused = node_control(&from_worker).await.expect_err("no pin");
        assert_authentication_failure(&refused);

        membership.undeclare(WORKER_ID);
        let removed = node_control(&from_worker).await.expect_err("removed");
        assert!(is_not_a_member(&removed), "{removed:?}");
    }

    #[tokio::test]
    async fn removing_one_pin_keeps_the_streams_of_other_nodes() {
        let site = TestSite::new();
        let other = site.authority.issue(SITE_ID, "worker-two", true);
        let pins = PeerPins::default();
        pins.insert(WORKER_ID, site.worker_pin());
        pins.insert(
            "worker-two",
            PrivatePeerPolicy {
                role: PrivateNodeRole::Worker,
                certificate_sha256: CertificateFingerprint::for_certificate(&other.0),
            },
        );
        let manager = site
            .serve_manager_with_membership(
                pins.clone(),
                Arc::new(FakeMembership::declaring(["worker-two"])),
            )
            .await;
        let from_other = site.manager_client(manager, Some(&other)).await;
        let mut stream = NodeControlServiceClient::new(from_other.clone())
            .stream_node_control(futures_util::stream::pending())
            .await
            .expect("stream")
            .into_inner();

        pins.remove(WORKER_ID);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), stream.message())
                .await
                .is_err(),
            "the other node's stream stays open"
        );
    }

    #[tokio::test]
    async fn a_certificate_from_another_authority_is_never_told_it_is_not_a_member() {
        let site = TestSite::new();
        let stranger = SiteAuthority::new().issue(SITE_ID, WORKER_ID, true);
        let manager = site
            .serve_manager_with_membership(
                PeerPins::default(),
                Arc::new(FakeMembership::declaring([])),
            )
            .await;

        let refused = match site
            .client(
                manager,
                MANAGER_ID,
                &site.manager.0,
                Some(&site.identity(&stranger)),
            )
            .connect()
            .await
        {
            Ok(channel) => node_control(&channel).await.expect_err("refused"),
            Err(error) => Status::unavailable(error.to_string()),
        };

        assert!(!is_not_a_member(&refused), "{refused:?}");
    }

    #[test]
    fn pinned_peer_is_verified_at_an_address_absent_from_its_certificate() {
        let authority = TestAuthority::new();
        let leaf = authority.issue("node-one", |_| {});

        for dialed in ["10.0.0.8", "100.64.0.10", "manager.example"] {
            verify_pinned(&authority, &leaf, "node-one", &leaf, dialed)
                .unwrap_or_else(|error| panic!("dialing {dialed}: {error}"));
        }
    }

    #[test]
    fn pinned_peer_verification_rejects_pin_identity_authority_expiry_and_usage() {
        let authority = TestAuthority::new();
        let leaf = authority.issue("node-one", |_| {});
        let other_leaf = authority.issue("node-one", |_| {});
        let foreign = TestAuthority::new().issue("node-one", |_| {});
        let expired = authority.issue("node-one", |parameters| {
            parameters.not_before = OffsetDateTime::now_utc() - Duration::hours(2);
            parameters.not_after = OffsetDateTime::now_utc() - Duration::hours(1);
        });
        let client_only = authority.issue("node-one", |parameters| {
            parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        });

        for (label, presented, expected_node, pinned) in [
            ("wrong pin", &leaf, "node-one", &other_leaf),
            ("wrong node identity", &leaf, "node-two", &leaf),
            ("another site CA", &foreign, "node-one", &foreign),
            ("expired leaf", &expired, "node-one", &expired),
            (
                "no server-auth usage",
                &client_only,
                "node-one",
                &client_only,
            ),
        ] {
            assert!(
                verify_pinned(&authority, presented, expected_node, pinned, "10.0.0.7").is_err(),
                "{label} must be rejected"
            );
        }
    }

    /// Verifies `presented` as a peer pinned to `pinned` and claiming `expected_node`, dialed at
    /// `dialed`.
    fn verify_pinned(
        authority: &TestAuthority,
        presented: &[u8],
        expected_node: &str,
        pinned: &[u8],
        dialed: &str,
    ) -> Result<(), rustls::Error> {
        let verifier = PinnedPeerVerifier::new(
            authority.certificate.der(),
            "site-one",
            expected_node,
            CertificateFingerprint::for_certificate(pinned),
        )
        .expect("pinned peer verifier");
        verifier
            .verify_server_cert(
                &CertificateDer::from(presented),
                &[],
                &ServerName::try_from(dialed.to_owned()).expect("server name"),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    struct TestAuthority {
        certificate: rcgen::Certificate,
        issuer: Issuer<'static, KeyPair>,
    }

    impl TestAuthority {
        fn new() -> Self {
            let mut parameters = CertificateParams::new(Vec::<String>::new()).expect("CA params");
            parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
            parameters.key_usages = vec![
                KeyUsagePurpose::DigitalSignature,
                KeyUsagePurpose::KeyCertSign,
                KeyUsagePurpose::CrlSign,
            ];
            let key = KeyPair::generate().expect("CA key");
            let certificate = parameters.self_signed(&key).expect("CA certificate");
            Self {
                certificate,
                issuer: Issuer::new(parameters, key),
            }
        }

        /// Issues a site leaf naming `node_id` that also carries the address `10.0.0.7`, so any
        /// other dialed address differs from every name in the certificate.
        fn issue(&self, node_id: &str, update: impl FnOnce(&mut CertificateParams)) -> Vec<u8> {
            let mut parameters =
                CertificateParams::new(vec!["10.0.0.7".to_owned()]).expect("leaf params");
            parameters.subject_alt_names.push(SanType::URI(
                certificate_identity_uri("site-one", node_id)
                    .expect("node claim")
                    .try_into()
                    .expect("node claim IA5"),
            ));
            parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            parameters.extended_key_usages = vec![
                ExtendedKeyUsagePurpose::ServerAuth,
                ExtendedKeyUsagePurpose::ClientAuth,
            ];
            parameters.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
            parameters.not_after = OffsetDateTime::now_utc() + Duration::hours(1);
            update(&mut parameters);
            parameters
                .signed_by(&KeyPair::generate().expect("leaf key"), &self.issuer)
                .expect("leaf certificate")
                .der()
                .to_vec()
        }
    }

    /// A test site with one manager and one worker, each with its own site-issued leaf.
    struct TestSite {
        authority: SiteAuthority,
        manager: (Vec<u8>, Vec<u8>),
        worker: (Vec<u8>, Vec<u8>),
    }

    impl TestSite {
        fn new() -> Self {
            let authority = SiteAuthority::new();
            Self {
                manager: authority.issue(SITE_ID, MANAGER_ID, true),
                worker: authority.issue(SITE_ID, WORKER_ID, true),
                authority,
            }
        }

        /// The manager's private configuration sharing `pins`.
        fn manager_configuration(
            &self,
            pins: PeerPins,
        ) -> dotcore_site::private_transport::PrivateServerConfiguration {
            let mut configuration = self.authority.private_configuration(
                SITE_ID,
                MANAGER_ID,
                PrivateNodeRole::Manager,
                self.manager.clone(),
                [],
            );
            configuration.peers = pins;
            configuration
        }

        /// Serves the manager's private listener with `pins` and `gate`, and a join service when
        /// one is given.
        async fn serve_manager(
            &self,
            pins: PeerPins,
            gate: JoinGate,
            joins: Option<FakeJoinHost>,
        ) -> SocketAddr {
            let mut routes = tonic::service::Routes::builder();
            if let Some(joins) = joins {
                routes.add_service(NodeJoinServiceServer::new(joins));
            }
            let mut listener = PrivateListener::new(
                self.manager_configuration(pins),
                routes.routes(),
                &Timing::test(),
            );
            listener.join_gate = gate;
            serve(listener).await
        }

        /// Serves the manager's private listener with `pins`, an open node control service, and
        /// `membership` telling which nodes the site declares.
        async fn serve_manager_with_membership(
            &self,
            pins: PeerPins,
            membership: Arc<FakeMembership>,
        ) -> SocketAddr {
            let mut routes = tonic::service::Routes::builder();
            routes.add_service(NodeControlServiceServer::new(OpenNodeControl));
            let mut listener = PrivateListener::new(
                self.manager_configuration(pins),
                routes.routes(),
                &Timing::test(),
            );
            listener.membership = Some(membership);
            serve(listener).await
        }

        /// The worker's pin as the manager records it.
        fn worker_pin(&self) -> PrivatePeerPolicy {
            PrivatePeerPolicy {
                role: PrivateNodeRole::Worker,
                certificate_sha256: CertificateFingerprint::for_certificate(&self.worker.0),
            }
        }

        /// Serves the manager's private listener with `joins` while `gate` is open, bounding
        /// connections without a pinned certificate to `max_open` and `idle_timeout`.
        async fn serve_manager_with(
            &self,
            gate: JoinGate,
            joins: FakeJoinHost,
            max_open: usize,
            idle_timeout: std::time::Duration,
        ) -> SocketAddr {
            let mut routes = tonic::service::Routes::builder();
            routes.add_service(NodeJoinServiceServer::new(joins));
            let mut listener = PrivateListener::new(
                self.manager_configuration(PeerPins::default()),
                routes.routes(),
                &Timing::test(),
            );
            listener.join_gate = gate;
            listener.unpinned = UnpinnedConnectionLimits {
                max_open,
                idle_timeout,
            };
            serve(listener).await
        }

        /// Serves a worker listener with no services that pins the manager.
        async fn serve_worker(&self) -> SocketAddr {
            serve(PrivateListener::new(
                self.authority.private_configuration(
                    SITE_ID,
                    WORKER_ID,
                    PrivateNodeRole::Worker,
                    self.worker.clone(),
                    [(
                        MANAGER_ID,
                        PrivateNodeRole::Manager,
                        self.manager.0.as_slice(),
                    )],
                ),
                tonic::service::Routes::default(),
                &Timing::test(),
            ))
            .await
        }

        /// Connects to the manager's listener, presenting `identity` when given.
        async fn manager_client(
            &self,
            address: SocketAddr,
            identity: Option<&(Vec<u8>, Vec<u8>)>,
        ) -> Channel {
            let identity = identity.map(|identity| self.identity(identity));
            self.client(address, MANAGER_ID, &self.manager.0, identity.as_ref())
                .connect()
                .await
                .expect("manager channel")
        }

        /// An endpoint for the listener of `node_id` presenting `certificate`, which presents
        /// `identity` when given.
        fn client(
            &self,
            address: SocketAddr,
            node_id: &str,
            certificate: &[u8],
            identity: Option<&Identity>,
        ) -> Endpoint {
            let verifier = PinnedPeerVerifier::new(
                self.authority.certificate.der(),
                SITE_ID,
                node_id,
                CertificateFingerprint::for_certificate(certificate),
            )
            .expect("peer verifier");
            let mut tls = ClientTlsConfig::new()
                .domain_name("127.0.0.1")
                .assume_http2(true);
            if let Some(identity) = identity {
                tls = tls.identity(identity.clone());
            }
            Endpoint::from_shared(format!("https://{address}"))
                .expect("peer endpoint")
                .tls_config_with_verifier(tls, Arc::new(verifier))
                .expect("peer client TLS")
        }

        /// The TLS identity of a leaf and its PKCS#8 key.
        fn identity(&self, (certificate, key): &(Vec<u8>, Vec<u8>)) -> Identity {
            Identity::from_pem(pem("CERTIFICATE", certificate), pem("PRIVATE KEY", key))
        }
    }

    /// A join service that records the connection each join request arrived on.
    #[derive(Clone, Default)]
    struct FakeJoinHost {
        connections: Arc<Mutex<Vec<PrivateConnectionInfo>>>,
    }

    impl FakeJoinHost {
        /// Each join's connection id and whether that connection presented a certificate.
        fn connections(&self) -> Vec<(u64, bool)> {
            self.infos()
                .iter()
                .map(|info| (info.id(), info.peer_certificate().is_some()))
                .collect()
        }

        /// The connection of each join.
        fn infos(&self) -> Vec<PrivateConnectionInfo> {
            self.connections.lock().expect("join connections").clone()
        }
    }

    #[tonic::async_trait]
    impl NodeJoinService for FakeJoinHost {
        async fn join(
            &self,
            request: Request<JoinRequest>,
        ) -> Result<Response<JoinResponse>, Status> {
            let connection = request
                .extensions()
                .get::<PrivateConnectionInfo>()
                .expect("every request carries its connection");
            self.connections
                .lock()
                .expect("join connections")
                .push(connection.clone());
            Ok(Response::new(JoinResponse::default()))
        }

        async fn commit(
            &self,
            _request: Request<CommitRequest>,
        ) -> Result<Response<CommitResponse>, Status> {
            Err(Status::unimplemented("the fake join host takes no commit"))
        }
    }

    /// Nodes a test site declares, which the test changes.
    struct FakeMembership(Mutex<Vec<String>>);

    impl FakeMembership {
        fn declaring<const N: usize>(node_ids: [&str; N]) -> Self {
            Self(Mutex::new(node_ids.map(str::to_owned).to_vec()))
        }

        fn undeclare(&self, node_id: &str) {
            self.0
                .lock()
                .expect("declared nodes")
                .retain(|declared| declared != node_id);
        }
    }

    impl SiteMembership for FakeMembership {
        fn declares(&self, node_id: &str) -> bool {
            self.0
                .lock()
                .expect("declared nodes")
                .iter()
                .any(|declared| declared == node_id)
        }
    }

    /// A node control service whose streams stay open and silent.
    struct OpenNodeControl;

    #[tonic::async_trait]
    impl NodeControlService for OpenNodeControl {
        type StreamNodeControlStream = std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<StreamNodeControlResponse, Status>> + Send>,
        >;

        async fn stream_node_control(
            &self,
            _request: Request<tonic::Streaming<StreamNodeControlRequest>>,
        ) -> Result<Response<Self::StreamNodeControlStream>, Status> {
            Ok(Response::new(Box::pin(futures_util::stream::pending())))
        }
    }

    async fn serve(listener: PrivateListener) -> SocketAddr {
        let server = prepare(listener, std::future::pending())
            .await
            .expect("private listener");
        let address = server.address;
        tokio::spawn(server.serve());
        address
    }

    async fn join(channel: &Channel) -> Result<JoinResponse, Status> {
        NodeJoinServiceClient::new(channel.clone())
            .join(JoinRequest::default())
            .await
            .map(Response::into_inner)
    }

    /// Opens a node control stream and returns its status; nothing is sent on it.
    async fn node_control(channel: &Channel) -> Result<(), Status> {
        NodeControlServiceClient::new(channel.clone())
            .stream_node_control(futures_util::stream::pending())
            .await
            .map(|_| ())
    }

    fn assert_authentication_failure(status: &Status) {
        assert_eq!(status.code(), Code::Unauthenticated, "{status:?}");
        assert_eq!(status.message(), "private peer authentication failed");
    }
}
