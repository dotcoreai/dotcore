//! Issues short-lived site certificates for unit tests that exercise private mutual TLS, for
//! example a manager client dialing a worker listener on loopback.
//!
//! Leaves carry the exact SiteNode identity claim that private ingress and outbound pins check,
//! so tests can prove both the accepted identity and pin or identity mismatches. A loopback
//! mutual-TLS listener serves the client-side transport tests.

use rcgen::{
    BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer,
    KeyPair, KeyUsagePurpose, SanType,
};
use time::{Duration as TimeDuration, OffsetDateTime};

#[cfg(test)]
use crate::private_transport::pem;
use crate::{
    node::folder::{CertificateFingerprint, certificate_identity_uri},
    private_transport::{PrivateNodeRole, PrivatePeerPolicy, PrivateServerConfiguration},
};

/// A test site certificate authority that signs node leaves.
pub struct TestAuthority {
    pub certificate: Certificate,
    issuer: Issuer<'static, KeyPair>,
}

impl Default for TestAuthority {
    fn default() -> Self {
        Self::new()
    }
}

impl TestAuthority {
    /// Creates a new self-signed site authority.
    pub fn new() -> Self {
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

    /// Issues one node leaf and its PKCS#8 key; `serves_localhost` adds the loopback IP a
    /// listener on 127.0.0.1 presents.
    pub fn issue(
        &self,
        site_id: &str,
        node_id: &str,
        serves_localhost: bool,
    ) -> (Vec<u8>, Vec<u8>) {
        let mut parameters = CertificateParams::new(Vec::<String>::new()).expect("leaf params");
        if serves_localhost {
            parameters
                .subject_alt_names
                .push(SanType::IpAddress(std::net::Ipv4Addr::LOCALHOST.into()));
        }
        parameters.subject_alt_names.push(SanType::URI(
            certificate_identity_uri(site_id, node_id)
                .expect("identity URI")
                .try_into()
                .expect("IA5 identity URI"),
        ));
        parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        parameters.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ];
        parameters.not_before = OffsetDateTime::now_utc() - TimeDuration::hours(1);
        parameters.not_after = OffsetDateTime::now_utc() + TimeDuration::hours(1);
        let key = KeyPair::generate().expect("leaf key");
        let certificate = parameters
            .signed_by(&key, &self.issuer)
            .expect("leaf certificate");
        (certificate.der().to_vec(), key.serialize_der())
    }

    /// Builds one node's private configuration on an ephemeral loopback port, pinning each peer's
    /// exact certificate under its role.
    pub fn private_configuration<'a>(
        &self,
        site_id: &str,
        node_id: &str,
        role: PrivateNodeRole,
        (certificate_der, private_key_pkcs8): (Vec<u8>, Vec<u8>),
        peers: impl IntoIterator<Item = (&'a str, PrivateNodeRole, &'a [u8])>,
    ) -> PrivateServerConfiguration {
        PrivateServerConfiguration {
            bind_address: "127.0.0.1:0".parse().expect("loopback bind address"),
            site_id: site_id.to_owned(),
            local_node_id: node_id.to_owned(),
            local_role: role,
            certificate_der,
            private_key_pkcs8,
            trust_anchor_der: self.certificate.der().to_vec(),
            peers: peers
                .into_iter()
                .map(|(node_id, role, certificate_der)| {
                    (
                        node_id.to_owned(),
                        PrivatePeerPolicy {
                            role,
                            certificate_sha256: CertificateFingerprint::for_certificate(
                                certificate_der,
                            ),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// A loopback listener that presents one node's certificate and requires a client certificate
/// chained to the site trust anchor, for tests of the private transport clients. It checks no
/// peer pin: the owning server enforces those.
#[cfg(test)]
pub struct MutualTlsListener {
    pub address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(test)]
impl MutualTlsListener {
    /// Binds `configuration`'s address and serves `routes` until the listener is dropped.
    pub async fn start(
        configuration: &PrivateServerConfiguration,
        routes: tonic::service::Routes,
    ) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let listener = tokio::net::TcpListener::bind(configuration.bind_address)
            .await
            .expect("loopback listener");
        let address = listener.local_addr().expect("listener address");
        let tls = tonic::transport::ServerTlsConfig::new()
            .identity(tonic::transport::Identity::from_pem(
                pem("CERTIFICATE", &configuration.certificate_der),
                pem("PRIVATE KEY", &configuration.private_key_pkcs8),
            ))
            .client_ca_root(tonic::transport::Certificate::from_pem(pem(
                "CERTIFICATE",
                &configuration.trust_anchor_der,
            )));
        let server = tonic::transport::Server::builder()
            .tls_config(tls)
            .expect("listener TLS")
            .add_routes(routes)
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener));
        let task = tokio::spawn(async move {
            server.await.expect("mutual TLS listener");
        });
        Self { address, task }
    }
}

#[cfg(test)]
impl Drop for MutualTlsListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}
