//! Loads the one site-issued machine credential held by each provisioned node.
//! The site manager separately holds the signing key; this local identity only
//! authenticates a bound SiteNode and never grants management authority.

use std::{
    fs::{self, OpenOptions},
    io::{self, Read},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::Arc,
};

use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime},
    server::WebPkiClientVerifier,
    sign::CertifiedKey,
};
use x509_parser::{extensions::GeneralName, prelude::FromDer};

use crate::{
    manager_trust::SITE_TRUST_DIRECTORY,
    node::folder::{
        NODE_CERTIFICATE_FILE, NODE_PRIVATE_KEY_FILE, TRUST_ANCHOR_FILE, certificate_identity_uri,
    },
};

/// Validated node-local trust material returned without reopening credential paths.
/// It authenticates a SiteNode but does not carry management authority.
pub struct LocalNodeIdentity {
    pub trust_anchor: Vec<u8>,
    pub certificate: Vec<u8>,
    pub private_key: Vec<u8>,
}

impl LocalNodeIdentity {
    pub(crate) fn load(
        root: &Path,
        site_id: &str,
        node_id: &str,
    ) -> Result<Self, LocalNodeIdentityError> {
        let trust = root.join(SITE_TRUST_DIRECTORY);
        require_directory(&trust)?;
        let trust_anchor = read_artifact(&trust.join(TRUST_ANCHOR_FILE), TRUST_ANCHOR_FILE)?;
        let certificate = read_artifact(&trust.join(NODE_CERTIFICATE_FILE), NODE_CERTIFICATE_FILE)?;
        let private_key = read_artifact(&trust.join(NODE_PRIVATE_KEY_FILE), NODE_PRIVATE_KEY_FILE)?;
        validate_certificate(&trust_anchor, &certificate, site_id, node_id)?;
        validate_key_pair(&certificate, &private_key)?;
        Ok(Self {
            trust_anchor,
            certificate,
            private_key,
        })
    }
}

/// An unsafe or inconsistent node-local identity cannot be used by a private listener.
/// Diagnostics identify only the invalid artifact or property, never private-key bytes.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LocalNodeIdentityError {
    #[error("trust directory must be real and owner-only")]
    Directory,

    #[error("trust artifact {artifact} could not be read")]
    ReadArtifact {
        artifact: &'static str,
        #[source]
        source: io::Error,
    },

    #[error("trust artifact is not a regular file")]
    ArtifactNotRegular { artifact: &'static str },

    #[error("trust artifact is not owner-only")]
    ArtifactPermissions { artifact: &'static str, mode: u32 },

    #[error("node certificate is not valid DER X.509")]
    CertificateParse,

    #[error("node certificate does not match the declared node")]
    CertificateIdentity,

    #[error("node certificate is not trusted for its required use")]
    CertificateTrust,

    #[error("node private key is not valid supported PKCS#8")]
    PrivateKeyParse,

    #[error("node private key does not match the node certificate")]
    KeyMismatch,
}

fn require_directory(path: &Path) -> Result<(), LocalNodeIdentityError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| LocalNodeIdentityError::Directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(LocalNodeIdentityError::Directory);
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(LocalNodeIdentityError::Directory);
    }
    Ok(())
}

fn read_artifact(path: &Path, artifact: &'static str) -> Result<Vec<u8>, LocalNodeIdentityError> {
    let mut options = OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_NOFOLLOW);
    let mut file = options.open(path).map_err(|source| {
        if source.raw_os_error() == Some(libc::ELOOP) {
            return LocalNodeIdentityError::ArtifactNotRegular { artifact };
        }
        LocalNodeIdentityError::ReadArtifact { artifact, source }
    })?;
    let metadata = file
        .metadata()
        .map_err(|source| LocalNodeIdentityError::ReadArtifact { artifact, source })?;
    if !metadata.file_type().is_file() {
        return Err(LocalNodeIdentityError::ArtifactNotRegular { artifact });
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(LocalNodeIdentityError::ArtifactPermissions { artifact, mode });
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| LocalNodeIdentityError::ReadArtifact { artifact, source })?;
    Ok(bytes)
}

fn validate_certificate(
    anchor: &[u8],
    leaf: &[u8],
    site_id: &str,
    node_id: &str,
) -> Result<(), LocalNodeIdentityError> {
    let (trailing, certificate) = x509_parser::certificate::X509Certificate::from_der(leaf)
        .map_err(|_| LocalNodeIdentityError::CertificateParse)?;
    if !trailing.is_empty() {
        return Err(LocalNodeIdentityError::CertificateParse);
    }
    let expected = certificate_identity_uri(site_id, node_id)
        .map_err(|_| LocalNodeIdentityError::CertificateIdentity)?;
    let san = certificate
        .subject_alternative_name()
        .map_err(|_| LocalNodeIdentityError::CertificateParse)?
        .ok_or(LocalNodeIdentityError::CertificateIdentity)?;
    let identities = san
        .value
        .general_names
        .iter()
        .filter_map(|name| match name {
            GeneralName::URI(uri) if uri.starts_with("urn:dotcore:site:") => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    if identities.as_slice() != [expected.as_str()] {
        return Err(LocalNodeIdentityError::CertificateIdentity);
    }
    let usages = certificate
        .extended_key_usage()
        .map_err(|_| LocalNodeIdentityError::CertificateParse)?
        .ok_or(LocalNodeIdentityError::CertificateTrust)?;
    if !usages.value.server_auth || !usages.value.client_auth {
        return Err(LocalNodeIdentityError::CertificateTrust);
    }
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(anchor.to_vec()))
        .map_err(|_| LocalNodeIdentityError::CertificateTrust)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let client = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
        .build()
        .map_err(|_| LocalNodeIdentityError::CertificateTrust)?;
    client
        .verify_client_cert(&CertificateDer::from(leaf), &[], UnixTime::now())
        .map_err(|_| LocalNodeIdentityError::CertificateTrust)?;
    Ok(())
}

fn validate_key_pair(certificate: &[u8], private_key: &[u8]) -> Result<(), LocalNodeIdentityError> {
    let provider = rustls::crypto::ring::default_provider();
    let result = CertifiedKey::from_der(
        vec![CertificateDer::from(certificate.to_vec())],
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key.to_vec())),
        &provider,
    );
    match result {
        Ok(_) => Ok(()),
        Err(rustls::Error::InconsistentKeys(_)) => Err(LocalNodeIdentityError::KeyMismatch),
        Err(_) => Err(LocalNodeIdentityError::PrivateKeyParse),
    }
}
