//! The joining node's own identity key and the signing request that carries only its public
//! half to the manager. The private key never leaves the new machine.
//!
//! A declared node that joins again keeps its key and proves it holds it with a signing request
//! bound to the one join it is made for, so a captured request is useless with another token.

use std::sync::Arc;

use rcgen::{Attribute, CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use rustls::{
    ServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer},
};
use sha2::{Digest, Sha256};
use x509_parser::{
    certificate::X509Certificate, certification_request::X509CertificationRequest,
    cri_attributes::ParsedCriAttribute, oid_registry::OID_PKCS9_CHALLENGE_PASSWORD,
    prelude::FromDer, time::ASN1Time,
};

use super::JoinSecret;
use crate::node::folder::{CertificateFingerprint, SecretPkcs8Key};

/// An ECDSA P-256 node key held by the joining machine: freshly generated for a new node, or
/// loaded from the node folder of a node joining again.
pub struct NodeKey {
    key: KeyPair,
}

impl NodeKey {
    /// Generates a new P-256 key from the system random source.
    pub fn generate() -> Result<Self, NodeKeyError> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).map_err(NodeKeyError::Generate)?;
        Ok(Self { key })
    }

    /// Loads a P-256 key from its PKCS#8 DER form, as a node folder stores it.
    pub fn from_pkcs8(private_key: &[u8]) -> Result<Self, NodeKeyError> {
        let key = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(private_key.to_vec()),
            &PKCS_ECDSA_P256_SHA256,
        )
        .map_err(NodeKeyError::Load)?;
        Ok(Self { key })
    }

    /// Builds the DER signing request for this key. It states no subject or names: the manager
    /// chooses every certificate value.
    pub fn csr_der(&self) -> Result<Vec<u8>, NodeKeyError> {
        let request = CertificateParams::default()
            .serialize_request(&self.key)
            .map_err(NodeKeyError::Request)?;
        Ok(request.der().to_vec())
    }

    /// Builds the DER signing request a declared node sends to join again with `secret`. Its
    /// challenge password is derived from the secret, so the request proves possession of this
    /// key for this join only.
    pub fn rejoin_csr_der(&self, secret: &JoinSecret) -> Result<Vec<u8>, NodeKeyError> {
        let challenge = Attribute {
            oid: CHALLENGE_PASSWORD_OID,
            values: der_set_of_utf8_string(&rejoin_challenge(secret)),
        };
        let request = CertificateParams::default()
            .serialize_request_with_attributes(&self.key, vec![challenge])
            .map_err(NodeKeyError::Request)?;
        Ok(request.der().to_vec())
    }

    /// Returns the PKCS#8 private key for owner-only storage in the node folder.
    pub fn private_key(&self) -> SecretPkcs8Key {
        SecretPkcs8Key::new(self.key.serialize_der())
    }

    /// Fingerprint the operator can compare with the one the manager shows for the request.
    pub fn fingerprint(&self) -> CertificateFingerprint {
        key_fingerprint(self.key.public_key_raw())
    }

    /// Builds the TLS server configuration of the joining node's dial-back listener: TLS 1.3
    /// only, no client authentication, and a self-signed certificate of this key, so each
    /// handshake proves to the dialer that this node holds the key its signing request carries.
    pub(super) fn dial_back_server_config(&self) -> Result<ServerConfig, NodeKeyError> {
        let certificate = CertificateParams::default()
            .self_signed(&self.key)
            .map_err(NodeKeyError::Certificate)?;
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_protocol_versions(&[&rustls::version::TLS13])
            .and_then(|builder| {
                builder.with_no_client_auth().with_single_cert(
                    vec![CertificateDer::from(certificate.der().to_vec())],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.serialize_der())),
                )
            })
            .map_err(NodeKeyError::ListenerIdentity)
    }

    /// Reports whether `certificate_der` certifies this key, so the node never installs a
    /// leaf issued for some other key.
    pub fn certifies(&self, certificate_der: &[u8]) -> bool {
        X509Certificate::from_der(certificate_der).is_ok_and(|(remainder, certificate)| {
            remainder.is_empty()
                && certificate.public_key().subject_public_key.data.as_ref()
                    == self.key.public_key_raw()
        })
    }
}

/// Computes the fingerprint of the public key inside a signing request without trusting the
/// request: it only parses the structure, and signing verifies the proof of possession.
pub fn csr_key_fingerprint(csr_der: &[u8]) -> Result<CertificateFingerprint, NodeKeyError> {
    Ok(key_fingerprint(&csr_public_key(csr_der)?))
}

/// Returns the raw public key inside a signing request, without trusting the request: it only
/// parses the structure.
pub(super) fn csr_public_key(csr_der: &[u8]) -> Result<Vec<u8>, NodeKeyError> {
    let (remainder, request) =
        X509CertificationRequest::from_der(csr_der).map_err(|_| NodeKeyError::MalformedRequest)?;
    if !remainder.is_empty() {
        return Err(NodeKeyError::MalformedRequest);
    }
    Ok(request
        .certification_request_info
        .subject_pki
        .subject_public_key
        .data
        .to_vec())
}

/// Checks that a signing request is well formed and signed by its own key, and returns that raw
/// public key, which the request thereby proves its sender holds.
pub(super) fn verified_csr_key(csr_der: &[u8]) -> Result<Vec<u8>, NodeKeyError> {
    let (remainder, request) =
        X509CertificationRequest::from_der(csr_der).map_err(|_| NodeKeyError::MalformedRequest)?;
    if !remainder.is_empty() {
        return Err(NodeKeyError::MalformedRequest);
    }
    request
        .verify_signature()
        .map_err(|_| NodeKeyError::RequestSignature)?;
    Ok(request
        .certification_request_info
        .subject_pki
        .subject_public_key
        .data
        .to_vec())
}

/// Checks a rejoin signing request: well formed, signed by its own key, and bound to `secret`.
/// Returns the raw public key it proves possession of.
pub(super) fn rejoin_request_key(
    csr_der: &[u8],
    secret: &JoinSecret,
) -> Result<Vec<u8>, NodeKeyError> {
    let (remainder, request) =
        X509CertificationRequest::from_der(csr_der).map_err(|_| NodeKeyError::MalformedRequest)?;
    if !remainder.is_empty() {
        return Err(NodeKeyError::MalformedRequest);
    }
    request
        .verify_signature()
        .map_err(|_| NodeKeyError::RequestSignature)?;
    let info = &request.certification_request_info;
    let mut challenges = info
        .iter_attributes()
        .filter(|attribute| attribute.oid == OID_PKCS9_CHALLENGE_PASSWORD);
    let bound = match (challenges.next(), challenges.next()) {
        (Some(attribute), None) => matches!(
            attribute.parsed_attribute(),
            ParsedCriAttribute::ChallengePassword(challenge)
                if challenge.0 == rejoin_challenge(secret)
        ),
        _ => false,
    };
    if !bound {
        return Err(NodeKeyError::RequestSession);
    }
    Ok(info.subject_pki.subject_public_key.data.to_vec())
}

/// Returns the raw public key a DER certificate certifies, or `None` when it does not parse.
pub(super) fn certificate_key(certificate_der: &[u8]) -> Option<Vec<u8>> {
    let (remainder, certificate) = X509Certificate::from_der(certificate_der).ok()?;
    remainder
        .is_empty()
        .then(|| certificate.public_key().subject_public_key.data.to_vec())
}

/// Where the current time falls in a certificate's validity period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CertificateTime {
    Valid,

    /// The period has not started, which usually means a clock is behind.
    NotYetValid,

    Expired,
}

/// Places the current time in the validity period of `certificate_der`; `None` when it does not
/// parse.
pub(super) fn certificate_time(certificate_der: &[u8]) -> Option<CertificateTime> {
    let (_, certificate) = X509Certificate::from_der(certificate_der).ok()?;
    let validity = certificate.validity();
    let now = ASN1Time::now();
    Some(if now < validity.not_before {
        CertificateTime::NotYetValid
    } else if now > validity.not_after {
        CertificateTime::Expired
    } else {
        CertificateTime::Valid
    })
}

/// A node key or signing request could not be produced or read.
#[derive(Debug, thiserror::Error)]
pub enum NodeKeyError {
    #[error("could not generate the node key")]
    Generate(#[source] rcgen::Error),

    #[error("could not load the node key; it must be an ECDSA P-256 PKCS#8 key")]
    Load(#[source] rcgen::Error),

    #[error("could not build the certificate signing request")]
    Request(#[source] rcgen::Error),

    #[error("could not build the node's self-signed listener certificate")]
    Certificate(#[source] rcgen::Error),

    #[error("could not present the node key on the dial-back listener")]
    ListenerIdentity(#[source] rustls::Error),

    #[error("the certificate signing request is not a well-formed DER structure")]
    MalformedRequest,

    #[error("the certificate signing request is not signed by its own key")]
    RequestSignature,

    #[error("the certificate signing request was not made for this join")]
    RequestSession,
}

/// PKCS#9 challenge password attribute (1.2.840.113549.1.9.7).
const CHALLENGE_PASSWORD_OID: &[u64] = &[1, 2, 840, 113549, 1, 9, 7];

/// Domain separation for the rejoin challenge, so the value means nothing outside a rejoin.
const REJOIN_CHALLENGE_CONTEXT: &[u8] = b"dotcore node rejoin v1\0";

/// SHA-256 over the raw public-key bytes (the uncompressed EC point for P-256).
fn key_fingerprint(public_key: &[u8]) -> CertificateFingerprint {
    CertificateFingerprint::from_bytes(Sha256::digest(public_key).into())
}

/// The challenge a rejoin request carries for `secret`: the hexadecimal SHA-256 of the secret
/// under the rejoin context, which reveals nothing about the secret itself.
fn rejoin_challenge(secret: &JoinSecret) -> String {
    let digest = Sha256::new()
        .chain_update(REJOIN_CHALLENGE_CONTEXT)
        .chain_update(secret.as_bytes())
        .finalize();
    CertificateFingerprint::from_bytes(digest.into())
        .as_str()
        .to_owned()
}

/// DER encoding of `SET { UTF8String value }`, the attribute values a challenge password takes.
/// Short-form lengths suffice for the fixed 64-character challenge.
fn der_set_of_utf8_string(value: &str) -> Vec<u8> {
    let length = u8::try_from(value.len())
        .ok()
        .filter(|length| *length < 0x7e)
        .expect("the challenge fits a short-form DER length");
    let mut encoded = vec![0x31, length + 2, 0x0c, length];
    encoded.extend_from_slice(value.as_bytes());
    encoded
}

#[cfg(test)]
mod tests {
    use std::fs;

    use rcgen::KeyPair;
    use rustls::pki_types::PrivatePkcs8KeyDer;

    use super::*;
    use crate::{membership::join::JOIN_SECRET_BYTES, node::folder::write_owner_only_pkcs8};

    #[test]
    fn signed_leaf_certifies_the_node_key_and_no_other() {
        let fixture = super::super::test_fixtures::initialized_site();
        let trust = crate::manager_trust::ManagerTrust::load(&fixture.root, &fixture.site)
            .expect("manager trust");
        let node_key = NodeKey::generate().expect("node key");

        let leaf = trust
            .sign_csr(
                &fixture.site.site_id,
                "node-worker",
                &node_key.csr_der().expect("csr"),
            )
            .expect("sign");

        assert!(node_key.certifies(&leaf));
        assert!(!NodeKey::generate().expect("other").certifies(&leaf));
        assert!(!node_key.certifies(&trust.manager_certificate));
    }

    #[test]
    fn csr_carries_the_node_key_fingerprint() {
        let key = NodeKey::generate().expect("key");
        let csr = key.csr_der().expect("csr");

        assert_eq!(
            csr_key_fingerprint(&csr).expect("fingerprint"),
            key.fingerprint()
        );
        assert_ne!(
            NodeKey::generate().expect("other").fingerprint(),
            key.fingerprint()
        );
    }

    #[test]
    fn stored_private_key_matches_the_requested_public_key() {
        let key = NodeKey::generate().expect("key");
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("directory");
        let path = directory.path().join("node-key.pk8");

        write_owner_only_pkcs8(&path, key.private_key()).expect("write key");

        let reloaded = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(fs::read(&path).expect("key bytes")),
            &PKCS_ECDSA_P256_SHA256,
        )
        .expect("reload");
        assert_eq!(reloaded.public_key_raw(), key.key.public_key_raw());
    }

    #[test]
    fn a_rejoin_request_proves_the_loaded_key_for_its_own_join_only() {
        let key = NodeKey::generate().expect("key");
        let loaded = NodeKey::from_pkcs8(&key.key.serialize_der()).expect("load key");
        let secret = JoinSecret::from_bytes([3; JOIN_SECRET_BYTES]);
        let other = JoinSecret::from_bytes([4; JOIN_SECRET_BYTES]);
        let csr = loaded.rejoin_csr_der(&secret).expect("rejoin csr");

        assert_eq!(
            rejoin_request_key(&csr, &secret).expect("bound request"),
            key.key.public_key_raw()
        );
        assert!(matches!(
            rejoin_request_key(&csr, &other),
            Err(NodeKeyError::RequestSession)
        ));
        assert!(matches!(
            rejoin_request_key(&key.csr_der().expect("csr"), &secret),
            Err(NodeKeyError::RequestSession)
        ));
        let mut tampered = csr.clone();
        *tampered.last_mut().expect("signature byte") ^= 0xff;
        assert!(matches!(
            rejoin_request_key(&tampered, &secret),
            Err(NodeKeyError::RequestSignature | NodeKeyError::MalformedRequest)
        ));
        assert!(matches!(
            NodeKey::from_pkcs8(b"not a key"),
            Err(NodeKeyError::Load(_))
        ));
    }

    #[test]
    fn malformed_requests_are_reported() {
        assert!(matches!(
            csr_key_fingerprint(b"not a csr"),
            Err(NodeKeyError::MalformedRequest)
        ));
        let mut csr = NodeKey::generate().expect("key").csr_der().expect("csr");
        csr.push(0);
        assert!(matches!(
            csr_key_fingerprint(&csr),
            Err(NodeKeyError::MalformedRequest)
        ));
    }
}
