//! Identity core of the mutually authenticated private transport between the SiteNodes of a site:
//! node roles, authenticated caller identities and their authorization, the private listener's
//! identity and peer policy, and outbound peer verification.
//!
//! A peer is bound by its exact pinned certificate and the SiteNode identity it claims, after its
//! certificate chains to the site trust anchor; the dialed address is never checked. The listener
//! that serves private services lives with the transport servers and uses this core at ingress.

use std::{collections::BTreeMap, net::SocketAddr, sync::Arc};

use base64::Engine as _;
use rustls::{
    DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
        verify_server_cert_signed_by_trust_anchor,
    },
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
use tonic::{Request, Status};
use x509_parser::{extensions::GeneralName, prelude::FromDer};

use crate::node::folder::{CertificateFingerprint, certificate_identity_uri};

/// Stable authenticated caller information made available to private component services.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedMachineIdentity {
    pub site_id: String,
    pub node_id: String,
    pub role: PrivateNodeRole,
}

/// Authorization role derived from exact configured identity, not from transport membership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateNodeRole {
    Manager,
    Worker,
}

/// Receiver-local context supplied while assembling private component services.
#[derive(Clone, Debug)]
pub struct PrivateServiceContext {
    receiver_role: PrivateNodeRole,
    receiver_node_id: Arc<str>,
}

impl PrivateServiceContext {
    /// Creates the context of the SiteNode `receiver_node_id` hosting private services as
    /// `receiver_role`.
    pub fn new(receiver_role: PrivateNodeRole, receiver_node_id: &str) -> Self {
        Self {
            receiver_role,
            receiver_node_id: Arc::from(receiver_node_id),
        }
    }

    /// Creates reusable authorization for one authenticated caller role at this receiver.
    pub fn authorize_role(&self, caller_role: PrivateNodeRole) -> AuthenticatedRoleAuthorization {
        AuthenticatedRoleAuthorization {
            receiver_role: self.receiver_role,
            receiver_node_id: Arc::clone(&self.receiver_node_id),
            caller_role,
        }
    }

    /// Returns the role of the SiteNode hosting these private services.
    pub fn receiver_role(&self) -> PrivateNodeRole {
        self.receiver_role
    }
}

/// Receiver-scoped authorization for an authenticated caller role.
///
/// A remote caller never claims the receiving SiteNode: that node reaches its own services in
/// process, so on the site manager no remote caller can act as the manager's SiteNode.
#[derive(Clone, Debug)]
pub struct AuthenticatedRoleAuthorization {
    receiver_role: PrivateNodeRole,
    receiver_node_id: Arc<str>,
    caller_role: PrivateNodeRole,
}

impl AuthenticatedRoleAuthorization {
    /// Authorizes a request after private ingress authentication and returns its exact identity.
    pub fn authorize<T>(
        &self,
        request: &Request<T>,
    ) -> Result<AuthenticatedMachineIdentity, Status> {
        let identity = request
            .extensions()
            .get::<AuthenticatedMachineIdentity>()
            .ok_or_else(authentication_failure)?;
        if identity.role != self.caller_role || *identity.node_id == *self.receiver_node_id {
            return Err(Status::permission_denied(
                "private operation is not authorized",
            ));
        }
        Ok(identity.clone())
    }

    /// Returns the role of the SiteNode receiving the authorized request.
    pub fn receiver_role(&self) -> PrivateNodeRole {
        self.receiver_role
    }
}

/// Complete private-listener identity and peer policy prepared by composition.
pub struct PrivateServerConfiguration {
    pub bind_address: SocketAddr,
    pub site_id: String,
    pub local_node_id: String,
    pub local_role: PrivateNodeRole,
    pub certificate_der: Vec<u8>,
    pub private_key_pkcs8: Vec<u8>,
    pub trust_anchor_der: Vec<u8>,
    pub peers: BTreeMap<String, PrivatePeerPolicy>,
}

impl std::fmt::Debug for PrivateServerConfiguration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PrivateServerConfiguration")
            .field("bind_address", &self.bind_address)
            .field("site_id", &self.site_id)
            .field("local_node_id", &self.local_node_id)
            .field("local_role", &self.local_role)
            .field("certificate_der", &"<DER certificate>")
            .field("private_key_pkcs8", &"<redacted>")
            .field("trust_anchor_der", &"<DER certificate>")
            .field("peers", &self.peers)
            .finish()
    }
}

/// One exact peer admitted at private request ingress.
#[derive(Clone, Debug)]
pub struct PrivatePeerPolicy {
    pub role: PrivateNodeRole,
    pub certificate_sha256: CertificateFingerprint,
}

/// Runs one synchronous private-service commit section on the blocking pool, so a stalled lock or
/// fsync never occupies an async executor thread. The section always runs to completion: when the
/// caller disconnects, dropping the request only detaches the blocking task.
pub async fn run_commit_section<T: Send + 'static>(
    section: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(section)
        .await
        .map_err(|_| Status::internal("private commit section failed"))
}

/// Verifies an outbound private peer: its leaf must chain to the site trust anchor for server
/// authentication, match the exact pinned certificate fingerprint, and claim exactly the expected
/// SiteNode. The dialed address is not checked: the exact pin already binds one certificate.
#[derive(Debug)]
pub struct PinnedPeerVerifier {
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
    certificate_sha256: CertificateFingerprint,
    site_id: String,
    node_id: String,
}

impl PinnedPeerVerifier {
    pub fn new(
        trust_anchor_der: &[u8],
        site_id: &str,
        node_id: &str,
        certificate_sha256: CertificateFingerprint,
    ) -> anyhow::Result<Self> {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from(trust_anchor_der.to_vec()))?;
        Ok(Self {
            roots,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            certificate_sha256,
            site_id: site_id.to_owned(),
            node_id: node_id.to_owned(),
        })
    }
}

impl ServerCertVerifier for PinnedPeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        verify_site_server_chain(end_entity, intermediates, &self.roots, &self.provider, now)?;
        if CertificateFingerprint::for_certificate(end_entity.as_ref()) != self.certificate_sha256 {
            return Err(rustls::Error::General(
                "private peer certificate pin mismatch".to_owned(),
            ));
        }
        if exact_node_identity(end_entity.as_ref(), &self.site_id).as_deref()
            != Some(self.node_id.as_str())
        {
            return Err(rustls::Error::General(
                "private peer node identity mismatch".to_owned(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Verifies that a server leaf chains to `roots`, is currently valid, and permits server
/// authentication, without matching any name or address. Callers bind the peer by its exact pin
/// and SiteNode identity instead.
pub fn verify_site_server_chain(
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    roots: &RootCertStore,
    provider: &CryptoProvider,
    now: UnixTime,
) -> Result<(), rustls::Error> {
    verify_server_cert_signed_by_trust_anchor(
        &ParsedCertificate::try_from(end_entity)?,
        roots,
        intermediates,
        now,
        provider.signature_verification_algorithms.all,
    )
}

pub fn exact_node_identity(certificate_der: &[u8], site_id: &str) -> Option<String> {
    let (_, certificate) =
        x509_parser::certificate::X509Certificate::from_der(certificate_der).ok()?;
    let prefix = format!("urn:dotcore:site:{site_id}:node:");
    let identities = certificate
        .subject_alternative_name()
        .ok()?
        .into_iter()
        .flat_map(|extension| extension.value.general_names.iter())
        .filter_map(|name| match name {
            GeneralName::URI(uri) if uri.starts_with("urn:dotcore:site:") => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    let [identity] = identities.as_slice() else {
        return None;
    };
    let node_id = identity.strip_prefix(&prefix)?;
    (certificate_identity_uri(site_id, node_id).ok()?.as_str() == *identity)
        .then(|| node_id.to_owned())
}

pub fn authentication_failure() -> Status {
    Status::unauthenticated("private peer authentication failed")
}

pub fn pem(label: &str, der: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut output = format!("-----BEGIN {label}-----\n");
    for chunk in encoded.as_bytes().chunks(64) {
        output.push_str(std::str::from_utf8(chunk).expect("base64 is UTF-8"));
        output.push('\n');
    }
    output.push_str(&format!("-----END {label}-----\n"));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_caller_claiming_the_receiving_site_node_is_refused() {
        let authorization = PrivateServiceContext::new(PrivateNodeRole::Manager, "manager-node")
            .authorize_role(PrivateNodeRole::Worker);
        let request = |node_id: &str| {
            let mut request = Request::new(());
            request
                .extensions_mut()
                .insert(AuthenticatedMachineIdentity {
                    site_id: "site-one".to_owned(),
                    node_id: node_id.to_owned(),
                    role: PrivateNodeRole::Worker,
                });
            request
        };

        let refused = authorization
            .authorize(&request("manager-node"))
            .expect_err("caller claiming the manager node");
        let accepted = authorization
            .authorize(&request("worker-node"))
            .expect("worker caller");

        assert_eq!(refused.code(), tonic::Code::PermissionDenied);
        assert_eq!(refused.message(), "private operation is not authorized");
        assert_eq!(accepted.node_id, "worker-node");
    }
}
