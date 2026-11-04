//! Serves the mutually authenticated inter-node boundary on its own listener.
//! Native TLS establishes site trust, while request ingress pins the complete peer leaf and binds its
//! exact SiteNode identity before any private component service receives a request. The identity
//! core it authenticates with (roles, caller identities, peer policy and outbound verification) is
//! owned by the site's private transport.

use std::{
    collections::BTreeMap, future::Future, net::SocketAddr, pin::Pin, sync::Arc, time::Duration,
};

use dotcore_site::{
    node::folder::CertificateFingerprint,
    private_transport::{
        AuthenticatedMachineIdentity, PrivateNodeRole, PrivatePeerPolicy,
        PrivateServerConfiguration, PrivateServiceContext, authentication_failure,
        exact_node_identity, pem,
    },
};
use tonic::{
    Request, Status,
    service::InterceptorLayer,
    transport::{
        Certificate, Identity, ServerTlsConfig,
        server::{TcpConnectInfo, TlsConnectInfo},
    },
};

/// Interval of HTTP/2 pings the listener sends on every connection, so a long-lived stream from a
/// peer that vanished without closing its connection ends.
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Longest wait for a ping acknowledgement before the listener closes the connection.
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Binds and prepares a native-tonic mTLS listener with exact ingress authentication.
pub(crate) async fn prepare(
    configuration: PrivateServerConfiguration,
    routes: tonic::service::Routes,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<PreparedPrivateServer, super::ServerError> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let listener = tokio::net::TcpListener::bind(configuration.bind_address)
        .await
        .map_err(|source| super::ListenerError::Private {
            address: configuration.bind_address,
            source,
        })?;
    let address = listener
        .local_addr()
        .map_err(super::ListenerError::PrivateAddress)?;
    let tls = ServerTlsConfig::new()
        .identity(Identity::from_pem(
            pem("CERTIFICATE", &configuration.certificate_der),
            pem("PRIVATE KEY", &configuration.private_key_pkcs8),
        ))
        .client_ca_root(Certificate::from_pem(pem(
            "CERTIFICATE",
            &configuration.trust_anchor_der,
        )));
    let ingress = PrivateIngress::new(&configuration);
    // The server ignores its own TCP options for a supplied listener, so the sockets set no delay
    // here and small responses aren't held for the peer's acknowledgment.
    let incoming = tonic::transport::server::TcpIncoming::from(listener).with_nodelay(Some(true));
    let future = tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE_INTERVAL))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .tls_config(tls)
        .map_err(|source| super::ServerError::Private(anyhow::Error::new(source)))?
        .layer(InterceptorLayer::new(ingress))
        .add_routes(routes)
        .serve_with_incoming_shutdown(incoming, shutdown);
    Ok(PreparedPrivateServer {
        address,
        future: Box::pin(async move {
            future
                .await
                .map_err(|source| super::ServerError::Private(anyhow::Error::new(source)))
        }),
    })
}

#[derive(Clone)]
struct PrivateIngress {
    site_id: Arc<str>,
    peers: Arc<BTreeMap<String, PrivatePeerPolicy>>,
}

impl PrivateIngress {
    fn new(configuration: &PrivateServerConfiguration) -> Self {
        Self {
            site_id: Arc::from(configuration.site_id.as_str()),
            peers: Arc::new(configuration.peers.clone()),
        }
    }

    fn authenticate_leaf(
        &self,
        certificate_der: &[u8],
    ) -> Result<AuthenticatedMachineIdentity, Status> {
        let node_id = exact_node_identity(certificate_der, &self.site_id)
            .ok_or_else(authentication_failure)?;
        let policy = self
            .peers
            .get(&node_id)
            .ok_or_else(authentication_failure)?;
        if CertificateFingerprint::for_certificate(certificate_der) != policy.certificate_sha256 {
            return Err(authentication_failure());
        }
        Ok(AuthenticatedMachineIdentity {
            site_id: self.site_id.to_string(),
            node_id,
            role: policy.role,
        })
    }
}

impl tonic::service::Interceptor for PrivateIngress {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        let connect = request
            .extensions()
            .get::<TlsConnectInfo<TcpConnectInfo>>()
            .ok_or_else(authentication_failure)?;
        let certificates = connect.peer_certs().ok_or_else(authentication_failure)?;
        let leaf = certificates.first().ok_or_else(authentication_failure)?;
        request
            .extensions_mut()
            .insert(self.authenticate_leaf(leaf.as_ref())?);
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use dotcore_site::{
        node::folder::{CertificateFingerprint, certificate_identity_uri},
        private_transport::PinnedPeerVerifier,
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

    use super::{PrivateIngress, PrivateNodeRole, PrivatePeerPolicy};

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

    #[test]
    fn the_same_node_leaf_takes_its_role_from_exact_peer_policy() {
        let mut parameters = CertificateParams::new(vec!["127.0.0.2".to_owned()])
            .expect("node certificate parameters");
        parameters.subject_alt_names.push(SanType::URI(
            certificate_identity_uri("site-one", "node-one")
                .expect("node claim")
                .try_into()
                .expect("node claim IA5"),
        ));
        let leaf = parameters
            .self_signed(&KeyPair::generate().expect("node key"))
            .expect("node leaf")
            .der()
            .to_vec();
        let pin = CertificateFingerprint::for_certificate(&leaf);

        for role in [PrivateNodeRole::Worker, PrivateNodeRole::Manager] {
            let ingress = PrivateIngress {
                site_id: Arc::from("site-one"),
                peers: Arc::new(BTreeMap::from([(
                    "node-one".to_owned(),
                    PrivatePeerPolicy {
                        role,
                        certificate_sha256: pin.clone(),
                    },
                )])),
            };
            let authenticated = ingress
                .authenticate_leaf(&leaf)
                .expect("same pinned node leaf remains authenticated");
            assert_eq!(authenticated.site_id, "site-one");
            assert_eq!(authenticated.node_id, "node-one");
            assert_eq!(authenticated.role, role);
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
}
