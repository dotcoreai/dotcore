//! Establishes the manager's private machine identity when a site is created.
//! Worker enrollment reuses this custody to issue one worker identity without
//! replacing the manager certificate or its site-local signing authority.

use std::{fs, io, os::unix::fs::PermissionsExt, path::Path};

use dotcore_proto::dotcore::site::v1::Site;
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, SanType, SubjectPublicKeyInfo,
};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use time::{Duration, OffsetDateTime};
use x509_parser::{
    certification_request::X509CertificationRequest,
    oid_registry::{OID_EC_P256, OID_KEY_TYPE_EC_PUBLIC_KEY},
    prelude::FromDer,
};

use crate::{
    atomic,
    node::{
        folder::{SecretPkcs8Key, certificate_identity_uri, write_owner_only_pkcs8},
        identity::LocalNodeIdentity,
    },
};

/// Site-local directory retaining the manager's certificate authority and machine identity.
pub const SITE_TRUST_DIRECTORY: &str = "trust";
/// Site-local owner-only certificate-authority private key.
pub const SITE_CA_PRIVATE_KEY_FILE: &str = "site-ca-key.pk8";
/// Manager-owned directory of exact worker leaf-certificate pins.
pub const WORKER_PIN_DIRECTORY: &str = "worker-pins";

const CA_VALIDITY_YEARS: i64 = 20;
const LEAF_VALIDITY_YEARS: i64 = 10;

pub struct ManagerTrust {
    #[cfg(any(test, feature = "test-support"))]
    pub trust_anchor: Vec<u8>,
    #[cfg(any(test, feature = "test-support"))]
    pub manager_certificate: Vec<u8>,
    issuer: Issuer<'static, KeyPair>,
}

impl ManagerTrust {
    pub fn load(site_root: &Path, site: &Site) -> Result<Self, ManagerTrustError> {
        let identity = Self::validate_for_startup(site_root, site)?;
        let trust_anchor = identity.trust_anchor;
        #[cfg(any(test, feature = "test-support"))]
        let manager_certificate = identity.certificate;
        let trust_root = site_root.join(SITE_TRUST_DIRECTORY);
        let ca_key_bytes = read_owner_file(&trust_root.join(SITE_CA_PRIVATE_KEY_FILE))?;
        let ca_key = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(ca_key_bytes),
            &PKCS_ECDSA_P256_SHA256,
        )
        .map_err(|source| ManagerTrustError::Invalid(anyhow::Error::new(source)))?;
        let issuer = Issuer::from_ca_cert_der(&CertificateDer::from(trust_anchor.clone()), ca_key)
            .map_err(|source| ManagerTrustError::Invalid(anyhow::Error::new(source)))?;
        validate_ca_key(&trust_anchor, &issuer)?;
        Ok(Self {
            #[cfg(any(test, feature = "test-support"))]
            trust_anchor,
            #[cfg(any(test, feature = "test-support"))]
            manager_certificate,
            issuer,
        })
    }

    pub(super) fn validate_for_startup(
        site_root: &Path,
        site: &Site,
    ) -> Result<LocalNodeIdentity, ManagerTrustError> {
        let manager = manager_node(site)?;
        let trust_root = site_root.join(SITE_TRUST_DIRECTORY);
        require_owner_directory(&trust_root)?;
        require_owner_directory(&trust_root.join(WORKER_PIN_DIRECTORY))?;
        reject_legacy_manager_identity(&trust_root)?;
        let identity = LocalNodeIdentity::load(site_root, &site.site_id, &manager.node_id)
            .map_err(|source| ManagerTrustError::Invalid(anyhow::Error::new(source)))?;
        Ok(identity)
    }

    pub fn create(site_root: &Path, site: &Site) -> Result<(), ManagerTrustError> {
        let manager = manager_node(site)?;
        let trust_root = site_root.join(SITE_TRUST_DIRECTORY);
        create_owner_directory(&trust_root)?;
        let pins = trust_root.join(WORKER_PIN_DIRECTORY);
        create_owner_directory(&pins)?;

        let mut ca_parameters = CertificateParams::new(Vec::<String>::new())?;
        ca_parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_parameters.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        set_validity(&mut ca_parameters, CA_VALIDITY_YEARS);
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let ca_certificate = ca_parameters.self_signed(&ca_key)?;
        let issuer = Issuer::new(ca_parameters, ca_key);
        let (manager_certificate, manager_key) =
            issue_leaf(&issuer, &site.site_id, &manager.node_id)?;
        write_owner_file(
            &trust_root.join(crate::node::folder::TRUST_ANCHOR_FILE),
            ca_certificate.der(),
        )?;
        write_owner_only_pkcs8(
            &trust_root.join(SITE_CA_PRIVATE_KEY_FILE),
            SecretPkcs8Key::new(issuer.key().serialize_der()),
        )
        .map_err(|source| {
            ManagerTrustError::Storage(anyhow::Error::new(source).context("write site CA key"))
        })?;
        write_owner_file(
            &trust_root.join(crate::node::folder::NODE_CERTIFICATE_FILE),
            &manager_certificate,
        )?;
        write_owner_only_pkcs8(
            &trust_root.join(crate::node::folder::NODE_PRIVATE_KEY_FILE),
            SecretPkcs8Key::new(manager_key),
        )
        .map_err(|source| {
            ManagerTrustError::Storage(anyhow::Error::new(source).context("write manager key"))
        })?;
        sync_directory(&pins)?;
        sync_directory(&trust_root)?;
        Self::load(site_root, site)?;
        Ok(())
    }

    /// Issues a leaf with a manager-generated key, for tests that need another site node's
    /// credentials.
    #[cfg(test)]
    pub fn issue_worker(
        &self,
        site_id: &str,
        node_id: &str,
    ) -> Result<(Vec<u8>, Vec<u8>), ManagerTrustError> {
        issue_leaf(&self.issuer, site_id, node_id)
    }

    /// Signs a node's signing request like [`Self::sign_csr`], but valid only from `not_before`
    /// to `not_after`, for tests of expired or not yet valid node certificates.
    #[cfg(test)]
    pub fn sign_csr_valid_between(
        &self,
        site_id: &str,
        node_id: &str,
        csr_der: &[u8],
        not_before: OffsetDateTime,
        not_after: OffsetDateTime,
    ) -> Result<Vec<u8>, ManagerTrustError> {
        let (_, csr) = X509CertificationRequest::from_der(csr_der)
            .map_err(|_| invalid_csr("the request is not a well-formed PKCS#10 DER structure"))?;
        let public_key =
            SubjectPublicKeyInfo::from_der(csr.certification_request_info.subject_pki.raw)
                .map_err(|source| ManagerTrustError::InvalidCsr(anyhow::Error::new(source)))?;
        let mut parameters = leaf_parameters(site_id, node_id)?;
        parameters.not_before = not_before;
        parameters.not_after = not_after;
        Ok(parameters
            .signed_by(&public_key, &self.issuer)?
            .der()
            .to_vec())
    }

    /// Signs the public key of a node's certificate signing request with the site CA. Every
    /// requested subject, SAN, CA flag, usage, and validity is ignored: the leaf carries only the
    /// manager-chosen values, so the request can contribute nothing but its ECDSA P-256 key.
    pub fn sign_csr(
        &self,
        site_id: &str,
        node_id: &str,
        csr_der: &[u8],
    ) -> Result<Vec<u8>, ManagerTrustError> {
        let (remainder, csr) = X509CertificationRequest::from_der(csr_der)
            .map_err(|_| invalid_csr("the request is not a well-formed PKCS#10 DER structure"))?;
        if !remainder.is_empty() {
            return Err(invalid_csr("the request has trailing bytes"));
        }
        let key_info = &csr.certification_request_info.subject_pki;
        let is_p256 = key_info.algorithm.algorithm == OID_KEY_TYPE_EC_PUBLIC_KEY
            && key_info
                .algorithm
                .parameters
                .as_ref()
                .and_then(|parameters| parameters.as_oid().ok())
                .is_some_and(|curve| curve == OID_EC_P256);
        if !is_p256 {
            return Err(invalid_csr("the request key must be ECDSA P-256"));
        }
        csr.verify_signature().map_err(|_| {
            invalid_csr("the request signature does not verify against its own public key")
        })?;
        let public_key = SubjectPublicKeyInfo::from_der(key_info.raw)
            .map_err(|source| ManagerTrustError::InvalidCsr(anyhow::Error::new(source)))?;

        let certificate =
            leaf_parameters(site_id, node_id)?.signed_by(&public_key, &self.issuer)?;
        Ok(certificate.der().to_vec())
    }
}

pub fn validate_for_private_startup(
    site_root: &Path,
    site: &Site,
) -> anyhow::Result<LocalNodeIdentity> {
    ManagerTrust::validate_for_startup(site_root, site).map_err(anyhow::Error::new)
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerTrustError {
    #[error("manager credential issuance failed")]
    Issuance(#[from] rcgen::Error),

    #[error("certificate signing request is rejected")]
    InvalidCsr(#[source] anyhow::Error),

    #[error("stored manager trust is invalid")]
    Invalid(#[source] anyhow::Error),

    #[error("manager trust storage failed: {0}")]
    Storage(#[source] anyhow::Error),
}

fn manager_node(
    site: &Site,
) -> Result<&dotcore_proto::dotcore::site::v1::SiteNode, ManagerTrustError> {
    let mut managers = site.nodes.iter().filter(|node| node.site_manager.is_some());
    let manager = managers.next().ok_or_else(|| {
        ManagerTrustError::Invalid(anyhow::anyhow!("Site has no declared manager"))
    })?;
    if managers.next().is_some() {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "Site has multiple managers"
        )));
    }
    Ok(manager)
}

fn issue_leaf(
    issuer: &Issuer<'_, KeyPair>,
    site_id: &str,
    node_id: &str,
) -> Result<(Vec<u8>, Vec<u8>), ManagerTrustError> {
    let parameters = leaf_parameters(site_id, node_id)?;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let certificate = parameters.signed_by(&key, issuer)?;
    Ok((certificate.der().to_vec(), key.serialize_der()))
}

/// The one certificate-parameter set every node leaf carries, whether the manager generated the
/// key or a node supplied it in a signing request. The only name is the SiteNode identity URI:
/// peers authenticate by exact pin and identity, never by address, so a leaf stays valid when
/// the node's addresses change.
fn leaf_parameters(site_id: &str, node_id: &str) -> Result<CertificateParams, ManagerTrustError> {
    let mut parameters = CertificateParams::new(Vec::<String>::new())?;
    parameters.subject_alt_names.push(SanType::URI(
        certificate_identity_uri(site_id, node_id)
            .map_err(|source| ManagerTrustError::Invalid(anyhow::Error::new(source)))?
            .try_into()
            .map_err(|source| ManagerTrustError::Invalid(anyhow::Error::new(source)))?,
    ));
    parameters.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    parameters.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    set_validity(&mut parameters, LEAF_VALIDITY_YEARS);
    Ok(parameters)
}

fn invalid_csr(reason: &'static str) -> ManagerTrustError {
    ManagerTrustError::InvalidCsr(anyhow::anyhow!("{reason}"))
}

fn set_validity(parameters: &mut CertificateParams, years: i64) {
    parameters.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
    parameters.not_after = OffsetDateTime::now_utc() + Duration::days(365 * years);
}

fn reject_legacy_manager_identity(trust_root: &Path) -> Result<(), ManagerTrustError> {
    for name in ["manager.der", "manager-key.pk8"] {
        let path = trust_root.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
                    "Site trust contains an obsolete manager-only node identity artifact"
                )));
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(storage("inspect obsolete node identity", &path, source)),
        }
    }
    Ok(())
}

fn validate_ca_key(anchor: &[u8], issuer: &Issuer<'_, KeyPair>) -> Result<(), ManagerTrustError> {
    let (remainder, certificate) = x509_parser::certificate::X509Certificate::from_der(anchor)
        .map_err(|_| {
            ManagerTrustError::Invalid(anyhow::anyhow!("Site CA certificate is malformed"))
        })?;
    if !remainder.is_empty()
        || certificate.public_key().subject_public_key.data.as_ref()
            != issuer.key().public_key_raw()
    {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "Site CA signing key does not match its trust anchor"
        )));
    }
    Ok(())
}

fn create_owner_directory(path: &Path) -> Result<(), ManagerTrustError> {
    atomic::create_owner_directory(path)
        .map_err(|source| storage("create owner-only directory", path, source))
}

fn require_owner_directory(path: &Path) -> Result<(), ManagerTrustError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| storage("inspect trust directory", path, source))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "trust path is not a real directory"
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "trust directory is not owner-only"
        )));
    }
    Ok(())
}

fn read_owner_file(path: &Path) -> Result<Vec<u8>, ManagerTrustError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|source| storage("inspect trust artifact", path, source))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "trust artifact is not a regular file"
        )));
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(ManagerTrustError::Invalid(anyhow::anyhow!(
            "trust artifact is not owner-only"
        )));
    }
    fs::read(path).map_err(|source| storage("read trust artifact", path, source))
}

fn write_owner_file(path: &Path, bytes: &[u8]) -> Result<(), ManagerTrustError> {
    atomic::write_new_owner_file(path, bytes)
        .map_err(|source| storage("write trust artifact", path, source))
}

fn sync_directory(path: &Path) -> Result<(), ManagerTrustError> {
    dotcore_domain::fs_sync::sync_directory(path)
        .map_err(|source| storage("sync trust directory", path, source))
}

fn storage(operation: &str, path: &Path, source: io::Error) -> ManagerTrustError {
    ManagerTrustError::Storage(
        anyhow::Error::new(source).context(format!("{operation} {}", path.display())),
    )
}

#[cfg(test)]
mod tests {
    use base64::{Engine, engine::general_purpose::STANDARD};
    use dotcore_proto::dotcore::site::v1::{SiteManagerRole, SiteNode};
    use rcgen::{PKCS_ECDSA_P384_SHA384, PKCS_ED25519, PKCS_RSA_SHA256};
    use x509_parser::{
        certificate::X509Certificate, extensions::GeneralName,
        oid_registry::OID_X509_EXT_BASIC_CONSTRAINTS,
    };

    use super::*;

    /// RSA 2048 key in PKCS#8 form; the ring backend cannot generate RSA keys.
    const RSA_2048_PKCS8_BASE64: &str = "MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCzoTnhwH6LAAObCY1G/zOkzJlx53vVNMKyUz8K93K+pWzJj3nBGmvPDwtKW4+9/IG/UumknukN2CVZhK0mkm7csAtfJ8UcP2lVXkeWrY36mGNS5P3dPNTfTYUsz5c0rJBRLTX2P0kvpmCpAZEH4JaAcGXmp6wO6Lbqki30C9n/VKM0gF60dNZemISoAz9vmxA+qAGU+pE4p6R6ahPoJk7p8SjSdT11J+qdIS7tljsMiRPdKU4bIC+NFVwUUJF2Ja70+OMcBBh4BktxkoxJlWch9UixSABtjGcpSgeQeVIWiB4NGLIwdNOp9eehTl0wV6Qcm0TIysrzj9zQM5PnL2A9AgMBAAECggEAJ43y2DiCVPrjkF3112KmXV11U4zw6oWz8r8chH5xLLRyJF885Q37m+x45L6NRD1oV7g6/TZF5PfJ37C0zSfVFDEKzAEOqTX4bKU2V+JvP3WTd4M3uKIm0OY502CB8AYowEtXlNoiNEUrh0OySz0D/xPhCH23CFBryPL3/gOnFQ9QYwLNbXQHFeovVnaU/L0EjXPfSTCyPNLd9L8EZDwenTpmOTZMYBeTsEbxKKWeGsaTgJbsax0+aDbmpnzVqw01AK9Rs66VcNYhuv7DVUIP9lxBZV+z3+CEtMgRtIpwpOSL/t/IIZXHcE7s1QN4zLavITt9jSveJXMBCpcPRKSJiQKBgQDd0nNKlRJQd9XkkAGQBLJ9hyj8UgzEGJt35uaycYjmc3NyYHFC3ImpqWVcuAfRSp1Xy1VtYH4RrLtpf4/Ftk/RLajYwCdJJhkp9QX0tc6gVgCk0QRgYCmsWBk5JJkYNacNjWzt2+NeisgMKI1kxxNFHSRnyXIo3qZHWOHy4LIKlQKBgQDPTosbwTwCqIoTsZBL7znHQ/dMKguitdBjRfZESD1hDzVS71WVHJck54kdUfcdYOSDpr7sVZQdl+GEm74eco1Q7js62D7MqRmrJB51+EV3ciIjCIj2sac/ClnA1ejshltNqke9TJCfkySqdq1l+8NFlVvuFPntJO7KXg1JWGm9CQKBgQCmxggqZePMHhzxaBfUqMNtv0XgVuwELyAMSMzgEp5D7T86Giptg5P4vERaLn6qKzga/uQuhOKj9UM+cUqt8oEWGwccV3Px+jPnKLIepuZABAkd4YwxzXrneaEVJl/4JLaBaPgh6TWMKjl3crHDuCzTxLJPWdR7x9IqSDgwvCD/9QKBgEeYjr6VLA6W+qj2iClRg5joRj5NvxsAw+mlPfozIM+mmvn7ookPCLw8IsS3jCrfTcySCBpDLBZUYBSWXUDSuy+aN2aJTJcOR8ktfj+r8QHhrxx+VB8vF93Ty0+oEPuvI1VdW81q5f/Wp7Efr58rJjOf89Hz+KUIPlgxx1DvcgVRAoGBAMXFZsFwP6WWlmU4Vv1Y9OW5KOAcGZOZsWG5kgZMr16OEE93pzC9JD+VbeCCPGi+VkEWYauKYU78rH1xBUMti+lu7w1DUlgpudZNS1lgXlo/a5F1gSGtm0U3tWBppc3frkt22LMWyMSqbmSsLgNfCTQn75b1/g9cA1Vl65VkXddS";

    /// A site directory holding only the manager trust material of a one-node site.
    struct TrustFixture {
        _directory: tempfile::TempDir,
        root: std::path::PathBuf,
        site: Site,
    }

    fn manager_trust() -> (TrustFixture, ManagerTrust) {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("Site directory");
        let site = Site {
            site_id: "site-one".to_owned(),
            nodes: vec![SiteNode {
                node_id: "node-manager".to_owned(),
                host: "127.0.0.1".to_owned(),
                display_name: "Manager".to_owned(),
                site_manager: Some(SiteManagerRole {}),
                manager_host: String::new(),
            }],
            ..Default::default()
        };
        ManagerTrust::create(directory.path(), &site).expect("create manager trust");
        let trust = ManagerTrust::load(directory.path(), &site).expect("manager trust");
        let fixture = TrustFixture {
            root: directory.path().to_path_buf(),
            _directory: directory,
            site,
        };
        (fixture, trust)
    }

    fn hostile_csr(key: &KeyPair) -> Vec<u8> {
        let mut parameters =
            CertificateParams::new(vec!["attacker.example".to_owned()]).expect("parameters");
        parameters
            .subject_alt_names
            .push(SanType::IpAddress("10.9.9.9".parse().expect("ip")));
        parameters
            .distinguished_name
            .push(rcgen::DnType::CommonName, "evil");
        parameters.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        parameters.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::CodeSigning];
        parameters.not_before = OffsetDateTime::now_utc() - Duration::days(9000);
        parameters.not_after = OffsetDateTime::now_utc() + Duration::days(365 * 100);
        parameters
            .serialize_request(key)
            .expect("hostile request")
            .der()
            .to_vec()
    }

    struct Summary {
        subject: String,
        issuer: String,
        sans: Vec<String>,
        key_usage: String,
        extended_key_usage: String,
        is_ca: bool,
        validity_seconds: i64,
    }

    fn summarize(der: &[u8]) -> Summary {
        let (_, certificate) = X509Certificate::from_der(der).expect("certificate");
        let sans = certificate
            .subject_alternative_name()
            .expect("san extension")
            .expect("san present")
            .value
            .general_names
            .iter()
            .map(|name| match name {
                GeneralName::URI(uri) => format!("uri:{uri}"),
                GeneralName::IPAddress(bytes) => format!("ip:{bytes:?}"),
                other => format!("{other:?}"),
            })
            .collect();
        Summary {
            subject: certificate.subject().to_string(),
            issuer: certificate.issuer().to_string(),
            sans,
            key_usage: format!(
                "{:?}",
                certificate.key_usage().expect("ku").expect("ku").value
            ),
            extended_key_usage: format!(
                "{:?}",
                certificate
                    .extended_key_usage()
                    .expect("eku")
                    .expect("eku")
                    .value
            ),
            is_ca: certificate
                .get_extension_unique(&OID_X509_EXT_BASIC_CONSTRAINTS)
                .expect("unique")
                .is_some_and(|extension| {
                    matches!(
                        extension.parsed_extension(),
                        x509_parser::extensions::ParsedExtension::BasicConstraints(constraints)
                            if constraints.ca
                    )
                }),
            validity_seconds: certificate.validity().not_after.timestamp()
                - certificate.validity().not_before.timestamp(),
        }
    }

    #[test]
    fn sign_csr_applies_only_manager_values_and_certifies_the_requested_key() {
        let (fixture, trust) = manager_trust();
        let site_id = fixture.site.site_id.as_str();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key");

        let signed = trust
            .sign_csr(site_id, "node-worker", &hostile_csr(&key))
            .expect("sign");
        let (issued, _) = trust.issue_worker(site_id, "node-worker").expect("issue");

        let (signed_summary, issued_summary) = (summarize(&signed), summarize(&issued));
        assert_eq!(signed_summary.subject, issued_summary.subject);
        assert_eq!(signed_summary.issuer, issued_summary.issuer);
        assert_eq!(signed_summary.sans, issued_summary.sans);
        assert_eq!(signed_summary.key_usage, issued_summary.key_usage);
        assert_eq!(
            signed_summary.extended_key_usage,
            issued_summary.extended_key_usage
        );
        assert!(!signed_summary.is_ca && !issued_summary.is_ca);
        assert!(
            (signed_summary.validity_seconds - issued_summary.validity_seconds).abs() <= 2,
            "validity follows the manager's lifetime"
        );
        assert_eq!(
            signed_summary.sans,
            [format!("uri:urn:dotcore:site:{site_id}:node:node-worker")]
        );
        assert!(!signed_summary.subject.contains("evil"));

        let (_, signed_certificate) = X509Certificate::from_der(&signed).expect("signed");
        assert_eq!(
            signed_certificate
                .public_key()
                .subject_public_key
                .data
                .as_ref(),
            key.public_key_raw()
        );
        let (_, authority) = X509Certificate::from_der(&trust.trust_anchor).expect("anchor");
        signed_certificate
            .verify_signature(Some(authority.public_key()))
            .expect("leaf is signed by the site CA");
    }

    #[test]
    fn manager_starts_with_a_declared_host_absent_from_its_leaf() {
        let (fixture, _) = manager_trust();
        let mut site = fixture.site.clone();
        let manager = site
            .nodes
            .iter_mut()
            .find(|node| node.site_manager.is_some())
            .expect("manager node");
        manager.host = "100.64.0.10".to_owned();

        ManagerTrust::validate_for_startup(&fixture.root, &site)
            .expect("manager identity does not depend on its declared host");
    }

    #[test]
    fn sign_csr_rejects_other_key_types_bad_signatures_and_garbage() {
        let (fixture, trust) = manager_trust();
        let site_id = fixture.site.site_id.as_str();
        let rsa = KeyPair::from_pkcs8_der_and_sign_algo(
            &PrivatePkcs8KeyDer::from(STANDARD.decode(RSA_2048_PKCS8_BASE64).expect("rsa base64")),
            &PKCS_RSA_SHA256,
        )
        .expect("rsa key");
        let request = |key: &KeyPair| {
            CertificateParams::default()
                .serialize_request(key)
                .expect("request")
                .der()
                .to_vec()
        };
        let mut tampered = request(&KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key"));
        *tampered.last_mut().expect("signature byte") ^= 0xff;
        let mut trailing = request(&KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).expect("key"));
        trailing.push(0);

        for (label, csr) in [
            (
                "ECDSA P-384",
                request(&KeyPair::generate_for(&PKCS_ECDSA_P384_SHA384).expect("key")),
            ),
            ("RSA", request(&rsa)),
            (
                "Ed25519",
                request(&KeyPair::generate_for(&PKCS_ED25519).expect("key")),
            ),
            ("tampered signature", tampered),
            ("trailing bytes", trailing),
            ("garbage", b"not a csr".to_vec()),
        ] {
            assert!(
                matches!(
                    trust.sign_csr(site_id, "node-worker", &csr),
                    Err(ManagerTrustError::InvalidCsr(_))
                ),
                "{label} must be rejected"
            );
        }
    }
}
