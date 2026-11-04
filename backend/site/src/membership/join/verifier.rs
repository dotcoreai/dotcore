//! Authenticates the join host from the token alone, before the joining node holds any site
//! certificate. The host must present the exact manager certificate the token pins, issued for
//! the token's site and manager node by the site CA whose fingerprint the token carries.
//!
//! There is no fallback: any other server, including another valid node of the same site, fails
//! the TLS handshake before the join secret is sent.

use std::sync::{Arc, Mutex, PoisonError};

use rustls::{
    DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
};

use super::JoinToken;
use crate::{
    node::folder::CertificateFingerprint,
    private_transport::{exact_node_identity, verify_site_server_chain},
};

/// The manager and site CA certificates a successful handshake verified.
#[derive(Clone, Debug)]
pub(super) struct VerifiedJoinHost {
    pub(super) manager_certificate_der: Vec<u8>,
    pub(super) site_ca_der: Vec<u8>,
}

/// Server verifier pinned to one join token.
///
/// The dialed address is not checked against the certificate: a node may reach the manager by
/// another address than the one it declares, and the exact pin already binds the identity.
#[derive(Debug)]
pub(super) struct JoinHostVerifier {
    site_id: String,
    manager_node_id: String,
    manager_certificate_sha256: CertificateFingerprint,
    site_ca_sha256: CertificateFingerprint,
    provider: Arc<CryptoProvider>,
    verified: Mutex<Option<VerifiedJoinHost>>,
}

impl JoinHostVerifier {
    pub(super) fn new(token: &JoinToken) -> Self {
        Self {
            site_id: token.site_id.clone(),
            manager_node_id: token.manager_node_id.clone(),
            manager_certificate_sha256: token.manager_certificate_sha256.clone(),
            site_ca_sha256: token.site_ca_sha256.clone(),
            provider: Arc::new(rustls::crypto::ring::default_provider()),
            verified: Mutex::new(None),
        }
    }

    /// Returns the certificates of the last host that passed verification.
    pub(super) fn verified(&self) -> Option<VerifiedJoinHost> {
        self.verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl ServerCertVerifier for JoinHostVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if CertificateFingerprint::for_certificate(end_entity.as_ref())
            != self.manager_certificate_sha256
        {
            return Err(rustls::Error::General(
                "join host certificate does not match the token's manager pin".to_owned(),
            ));
        }
        let site_ca = intermediates
            .iter()
            .find(|certificate| {
                CertificateFingerprint::for_certificate(certificate.as_ref()) == self.site_ca_sha256
            })
            .ok_or_else(|| {
                rustls::Error::General(
                    "join host did not present the token's site CA certificate".to_owned(),
                )
            })?;
        let mut roots = RootCertStore::empty();
        roots.add(site_ca.clone().into_owned())?;
        verify_site_server_chain(end_entity, &[], &roots, &self.provider, now)?;
        if exact_node_identity(end_entity.as_ref(), &self.site_id).as_deref()
            != Some(self.manager_node_id.as_str())
        {
            return Err(rustls::Error::General(
                "join host certificate does not name the token's site manager".to_owned(),
            ));
        }

        *self.verified.lock().unwrap_or_else(PoisonError::into_inner) = Some(VerifiedJoinHost {
            manager_certificate_der: end_entity.as_ref().to_vec(),
            site_ca_der: site_ca.as_ref().to_vec(),
        });
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
