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
    UnpinnedConnectionLimits, authentication_failure,
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
/// connections, and what it admits from connections without a pinned certificate.
pub(crate) struct PrivateListener {
    pub(crate) configuration: PrivateServerConfiguration,
    pub(crate) routes: tonic::service::Routes,
    pub(crate) timing: PrivateTiming,
    pub(crate) unpinned: UnpinnedConnectionLimits,
    pub(crate) join_gate: JoinGate,
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
    let ingress = PrivateIngress::new(&configuration, join_gate);
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
/// identity it then carries to the private services.
#[derive(Clone)]
struct PrivateIngress {
    site_id: Arc<str>,
    pins: PeerPins,
    join_gate: JoinGate,
}

impl PrivateIngress {
    fn new(configuration: &PrivateServerConfiguration, join_gate: JoinGate) -> Self {
        Self {
            site_id: Arc::from(configuration.site_id.as_str()),
            pins: configuration.peers.clone(),
            join_gate,
        }
    }

    /// Admits one request, attaching the authenticated identity of a pinned peer. Every refusal
    /// is the same authentication failure, so it reveals neither the pins nor whether a join is
    /// active.
    fn admit<B>(&self, request: &mut http::Request<B>) -> Result<(), Status> {
        if is_join_request(request) {
            return self
                .join_gate
                .is_open()
                .then_some(())
                .ok_or_else(authentication_failure);
        }
        let identity = request
            .extensions()
            .get::<PrivateConnectionInfo>()
            .and_then(PrivateConnectionInfo::peer_certificate)
            .and_then(|leaf| self.pins.authenticate(&self.site_id, leaf))
            .ok_or_else(authentication_failure)?;
        request.extensions_mut().insert(identity);
        Ok(())
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
        site::private::v1::node_control_service_client::NodeControlServiceClient,
    };
    use dotcore_site::{
        node::folder::{CertificateFingerprint, certificate_identity_uri},
        private_transport::{
            JoinGate, PeerPins, PinnedPeerVerifier, PrivateConnectionInfo, PrivatePeerPolicy, pem,
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
            let verifier = PinnedPeerVerifier::new(
                self.authority.certificate.der(),
                SITE_ID,
                MANAGER_ID,
                CertificateFingerprint::for_certificate(&self.manager.0),
            )
            .expect("manager verifier");
            let mut tls = ClientTlsConfig::new()
                .domain_name("127.0.0.1")
                .assume_http2(true);
            if let Some((certificate, key)) = identity {
                tls = tls.identity(Identity::from_pem(
                    pem("CERTIFICATE", certificate),
                    pem("PRIVATE KEY", key),
                ));
            }
            Endpoint::from_shared(format!("https://{address}"))
                .expect("manager endpoint")
                .tls_config_with_verifier(tls, Arc::new(verifier))
                .expect("manager client TLS")
                .connect()
                .await
                .expect("manager channel")
        }
    }

    /// A join service that records the connection each join request arrived on.
    #[derive(Clone, Default)]
    struct FakeJoinHost {
        connections: Arc<Mutex<Vec<(u64, bool)>>>,
    }

    impl FakeJoinHost {
        /// Each join's connection id and whether that connection presented a certificate.
        fn connections(&self) -> Vec<(u64, bool)> {
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
                .push((connection.id(), connection.peer_certificate().is_some()));
            Ok(Response::new(JoinResponse::default()))
        }

        async fn commit(
            &self,
            _request: Request<CommitRequest>,
        ) -> Result<Response<CommitResponse>, Status> {
            Err(Status::unimplemented("the fake join host takes no commit"))
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
