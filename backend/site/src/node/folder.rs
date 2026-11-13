//! Defines the node folder contract a joined worker node reads before its process starts: the
//! worker manifest, pinned manager certificate, and node-local trust files. The folder contains
//! only node-local bootstrap and pinned trust metadata; manager-owned site desired state is neither
//! copied nor made writable here.

use std::{
    fmt,
    fs::{self, OpenOptions, TryLockError},
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

use dotcore_domain::BindAddress;
use dotcore_domain::valid_id;
use dotcore_node_hardware::{MachineDescription, MachineError};
use dotcore_proto::dotcore::{
    node::v1::{DeclaredMachine, NodeObservationConfiguration},
    site::v1::ExecutionTarget,
};
use rustls::{
    RootCertStore,
    pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, UnixTime},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use x509_parser::{extensions::GeneralName, prelude::FromDer};

use crate::node::{
    identity::{LocalNodeIdentity, LocalNodeIdentityError},
    observation_configuration::{
        NodeObservationConfigurationDigest, NodeObservationConfigurationError,
        validate_node_observation_configuration,
    },
};
use crate::private_transport::verify_site_server_chain;

/// Name of the non-secret worker bootstrap manifest.
pub const WORKER_MANIFEST_FILE: &str = "worker.yaml";
/// Name of the site-local X.509 trust anchor in DER form.
pub const TRUST_ANCHOR_FILE: &str = "site-ca.der";
/// Name of the pinned manager leaf certificate in DER form.
pub(crate) const MANAGER_CERTIFICATE_FILE: &str = "manager.der";
/// Name of a local SiteNode's leaf certificate in DER form.
pub const NODE_CERTIFICATE_FILE: &str = "node.der";
/// Name of a local SiteNode's PKCS#8 private key artifact.
pub const NODE_PRIVATE_KEY_FILE: &str = "node-key.pk8";
/// File locked exclusively by the one process holding this node folder's node custody.
pub(crate) const WORKER_CUSTODY_LOCK_FILE: &str = ".node-custody.lock";

const IDENTITY_URI_PREFIX: &str = "urn:dotcore:site:";

/// Non-secret bootstrap information installed on exactly one worker node.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeFolderBootstrap {
    /// Site identity encoded into both peer certificates.
    pub site_id: String,
    /// SiteNode identity owned by this worker root.
    pub worker_node_id: String,
    /// Complete SHA-256 digest of the worker leaf certificate.
    pub worker_certificate_sha256: CertificateFingerprint,
    /// Pinned manager endpoint and identity.
    pub manager: NodeFolderManager,
    /// Complete immutable local target and observation policy projection.
    pub observation: NodeObservationConfiguration,
    /// Exact immutable target-owned configuration used for worker realization.
    pub realization_targets: Vec<ExecutionTarget>,
    /// Canonical digest of the installed observation configuration.
    pub observation_configuration_sha256: NodeObservationConfigurationDigest,
    /// Specific IPv4 address or host name and port the node's private listener binds, for
    /// example `10.0.0.7:7443` or `gpu-box.example.ts.net:7443`; a name is resolved when the node
    /// starts.
    pub private_bind_address: BindAddress,

    /// Hardware the node reports in place of its real hardware; absent when it reads its real
    /// hardware.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_machine: Option<DeclaredMachine>,
}

impl NodeFolderBootstrap {
    /// Creates a bootstrap manifest after validating its declared identifiers.
    pub fn new(
        site_id: impl Into<String>,
        worker_node_id: impl Into<String>,
        worker_certificate_sha256: CertificateFingerprint,
        manager: NodeFolderManager,
        observation: NodeObservationConfiguration,
        realization_targets: Vec<ExecutionTarget>,
        private_bind_address: BindAddress,
    ) -> Result<Self, NodeFolderContractError> {
        let observation_configuration_sha256 =
            NodeObservationConfigurationDigest::for_configuration(&observation)?;
        let bootstrap = Self {
            site_id: site_id.into(),
            worker_node_id: worker_node_id.into(),
            worker_certificate_sha256,
            manager,
            observation,
            realization_targets,
            observation_configuration_sha256,
            private_bind_address,
            declared_machine: None,
        };
        bootstrap.validate()?;
        Ok(bootstrap)
    }

    /// Declares the hardware the node reports in place of its real hardware, if any.
    pub fn with_declared_machine(
        mut self,
        declared_machine: Option<DeclaredMachine>,
    ) -> Result<Self, NodeFolderContractError> {
        self.declared_machine = declared_machine;
        self.declared_machine_description()?;
        Ok(self)
    }

    /// Returns the description of the declared machine, failing on a missing or invalid one.
    fn declared_machine_description(
        &self,
    ) -> Result<Option<MachineDescription>, NodeFolderContractError> {
        let Some(declared) = self.declared_machine.as_ref() else {
            return Ok(None);
        };
        let description = declared
            .description
            .as_ref()
            .ok_or(NodeFolderContractError::MissingMachineDescription)?;
        MachineDescription::from_stored(description)
            .map(Some)
            .map_err(NodeFolderContractError::DeclaredMachine)
    }

    fn validate(&self) -> Result<(), NodeFolderContractError> {
        validate_identifier("site", &self.site_id)?;
        validate_identifier("worker node", &self.worker_node_id)?;
        validate_identifier("manager node", &self.manager.node_id)?;
        self.worker_certificate_sha256.validate()?;
        self.manager.validate_endpoint()?;
        validate_node_observation_configuration(&self.observation)?;
        let observation_target_ids = self
            .observation
            .execution_targets
            .iter()
            .map(|target| target.execution_target_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let realization_targets = &self.realization_targets;
        let realization_target_ids = realization_targets
            .iter()
            .map(|target| target.execution_target_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        if realization_targets.is_empty()
            || realization_target_ids.len() != realization_targets.len()
            || observation_target_ids != realization_target_ids
            || realization_targets.iter().any(|target| {
                target.site_node_id != self.worker_node_id
                    || !valid_id(&target.execution_target_id)
                    || target.platform.is_none()
                    || target.runtime_binding.is_none()
            })
        {
            return Err(NodeFolderContractError::RealizationTargets);
        }
        if NodeObservationConfigurationDigest::for_configuration(&self.observation)?
            != self.observation_configuration_sha256
        {
            return Err(NodeFolderContractError::ObservationConfigurationDigest);
        }
        if self.private_bind_address.is_wildcard() || self.private_bind_address.port() == 0 {
            return Err(NodeFolderContractError::PrivateBindAddress);
        }
        self.declared_machine_description()?;
        self.manager.certificate_sha256.validate()
    }
}
/// Exact manager identity and private endpoint trusted by a worker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeFolderManager {
    /// Declared SiteNode identity of the one site manager.
    pub node_id: String,
    /// HTTPS endpoint used only by private inter-node services.
    pub private_endpoint: Url,
    /// Complete SHA-256 digest of the manager leaf certificate.
    pub certificate_sha256: CertificateFingerprint,
}

impl NodeFolderManager {
    /// Creates validated manager trust metadata.
    pub fn new(
        node_id: impl Into<String>,
        private_endpoint: impl AsRef<str>,
        certificate_sha256: CertificateFingerprint,
    ) -> Result<Self, NodeFolderContractError> {
        let private_endpoint = Url::parse(private_endpoint.as_ref())
            .map_err(NodeFolderContractError::ManagerEndpoint)?;
        let manager = Self {
            node_id: node_id.into(),
            private_endpoint,
            certificate_sha256,
        };
        validate_identifier("manager node", &manager.node_id)?;
        manager.validate_endpoint()?;
        manager.certificate_sha256.validate()?;
        Ok(manager)
    }

    fn validate_endpoint(&self) -> Result<(), NodeFolderContractError> {
        if self.private_endpoint.scheme() != "https"
            || self.private_endpoint.host_str().is_none()
            || self.private_endpoint.port().is_none()
            || !self.private_endpoint.username().is_empty()
            || self.private_endpoint.password().is_some()
            || self.private_endpoint.path() != "/"
            || self.private_endpoint.query().is_some()
            || self.private_endpoint.fragment().is_some()
        {
            return Err(NodeFolderContractError::PrivateManagerEndpoint);
        }
        Ok(())
    }
}

/// A complete lowercase SHA-256 certificate fingerprint.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CertificateFingerprint(String);

impl CertificateFingerprint {
    /// Computes a full SHA-256 fingerprint over one DER certificate.
    pub fn for_certificate(certificate_der: &[u8]) -> Self {
        let digest = Sha256::digest(certificate_der);
        Self(encode_hex(&digest))
    }

    /// Constructs a fingerprint from all 32 digest bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(encode_hex(&bytes))
    }

    /// Parses one complete canonical lowercase SHA-256 fingerprint.
    pub fn parse(value: impl Into<String>) -> Result<Self, NodeFolderContractError> {
        let fingerprint = Self(value.into());
        fingerprint.validate()?;
        Ok(fingerprint)
    }

    /// Returns the canonical lowercase hexadecimal fingerprint.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), NodeFolderContractError> {
        if self.0.len() != 64
            || !self
                .0
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(NodeFolderContractError::CertificateFingerprint);
        }
        Ok(())
    }
}

impl fmt::Debug for CertificateFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CertificateFingerprint")
            .field(&self.0)
            .finish()
    }
}

/// Exclusive process custody for one provisioned worker root.
#[derive(Debug)]
pub struct WorkerNodeCustody {
    file: fs::File,
}

impl WorkerNodeCustody {
    /// Acquires custody without waiting so a second process fails before workload preparation.
    pub fn try_acquire(root: &Path) -> Result<Self, WorkerNodeCustodyError> {
        let path = root.join(WORKER_CUSTODY_LOCK_FILE);
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let file = options
            .open(&path)
            .map_err(|source| WorkerNodeCustodyError::Storage {
                source: anyhow::Error::new(source)
                    .context(format!("open worker custody lock {}", path.display())),
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(WorkerNodeCustodyError::AlreadyHeld),
            Err(TryLockError::Error(source)) => {
                return Err(WorkerNodeCustodyError::Storage {
                    source: anyhow::Error::new(source)
                        .context(format!("lock worker custody {}", path.display())),
                });
            }
        }
        Ok(Self { file })
    }
}

impl Drop for WorkerNodeCustody {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// Failure to acquire exclusive custody of a provisioned worker root.
#[derive(Debug, thiserror::Error)]
pub enum WorkerNodeCustodyError {
    #[error("worker node custody is already held by another process")]
    AlreadyHeld,

    #[error("worker node custody storage failed")]
    Storage {
        #[source]
        source: anyhow::Error,
    },
}

/// Site and exact peer identities expected by this worker process.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeFolderExpectation {
    site_id: String,
    worker_node_id: String,
    manager_node_id: String,
}

impl NodeFolderExpectation {
    /// Defines identities which must match the manifest and both certificate claims.
    pub fn new(
        site_id: impl Into<String>,
        worker_node_id: impl Into<String>,
        manager_node_id: impl Into<String>,
    ) -> Self {
        Self {
            site_id: site_id.into(),
            worker_node_id: worker_node_id.into(),
            manager_node_id: manager_node_id.into(),
        }
    }
}

/// Validates a node folder before listener setup or runtime mutation.
#[derive(Clone, Debug)]
pub struct NodeFolderValidator {
    expected: NodeFolderExpectation,
}

impl NodeFolderValidator {
    /// Loads the node folder's declared identities and validates all credentials against them.
    pub fn validate_declared(
        root: &Path,
    ) -> Result<ValidatedNodeFolder, NodeFolderValidationError> {
        let bootstrap = load_bootstrap(root)?;
        Self::new(NodeFolderExpectation::new(
            &bootstrap.site_id,
            &bootstrap.worker_node_id,
            &bootstrap.manager.node_id,
        ))
        .validate(root)
    }

    /// Creates a validator pinned to the process's declared site and peer identities.
    pub fn new(expected: NodeFolderExpectation) -> Self {
        Self { expected }
    }

    /// Loads fixed node folder artifacts and validates trust, identity, and key correspondence.
    pub fn validate(&self, root: &Path) -> Result<ValidatedNodeFolder, NodeFolderValidationError> {
        let bootstrap = load_bootstrap(root)?;
        bootstrap
            .validate()
            .map_err(NodeFolderValidationError::InvalidBootstrap)?;
        self.validate_expected_identities(&bootstrap)?;

        reject_invalid_worker_identity_layout(root)?;
        let identity = LocalNodeIdentity::load(root, &bootstrap.site_id, &bootstrap.worker_node_id)
            .map_err(map_node_identity_error)?;
        let trust_anchor = identity.trust_anchor;
        let worker_certificate = identity.certificate;
        let worker_private_key = identity.private_key;
        let manager_certificate = read_artifact(root, MANAGER_CERTIFICATE_FILE)?;
        validate_certificate_parse("trust anchor", &trust_anchor)?;
        validate_certificate_parse("manager", &manager_certificate)?;
        validate_certificate_parse("worker", &worker_certificate)?;
        validate_fingerprints(&bootstrap, &manager_certificate, &worker_certificate)?;
        validate_certificate_claim(
            "manager",
            &manager_certificate,
            &bootstrap.site_id,
            &bootstrap.manager.node_id,
        )?;
        validate_trust(&trust_anchor, &manager_certificate)?;

        Ok(ValidatedNodeFolder {
            bootstrap,
            trust_anchor,
            manager_certificate,
            worker_certificate,
            worker_private_key,
        })
    }

    fn validate_expected_identities(
        &self,
        bootstrap: &NodeFolderBootstrap,
    ) -> Result<(), NodeFolderValidationError> {
        for (identity, expected, actual) in [
            (
                "site",
                self.expected.site_id.as_str(),
                bootstrap.site_id.as_str(),
            ),
            (
                "worker node",
                self.expected.worker_node_id.as_str(),
                bootstrap.worker_node_id.as_str(),
            ),
            (
                "manager node",
                self.expected.manager_node_id.as_str(),
                bootstrap.manager.node_id.as_str(),
            ),
        ] {
            if expected != actual {
                return Err(NodeFolderValidationError::IdentityMismatch { identity });
            }
        }
        Ok(())
    }
}

/// Fully validated worker bootstrap and mTLS artifacts.
pub struct ValidatedNodeFolder {
    bootstrap: NodeFolderBootstrap,
    trust_anchor: Vec<u8>,
    manager_certificate: Vec<u8>,
    worker_certificate: Vec<u8>,
    worker_private_key: Vec<u8>,
}

impl ValidatedNodeFolder {
    /// Returns the validated site identity.
    pub fn site_id(&self) -> &str {
        &self.bootstrap.site_id
    }

    /// Returns the exact SiteNode identity bound to this worker.
    pub fn worker_node_id(&self) -> &str {
        &self.bootstrap.worker_node_id
    }

    /// Returns the exact manager SiteNode identity trusted by this worker.
    pub fn manager_node_id(&self) -> &str {
        &self.bootstrap.manager.node_id
    }

    /// Returns the address or host name the private listener binds.
    pub fn private_bind_address(&self) -> &BindAddress {
        &self.bootstrap.private_bind_address
    }

    /// Returns the validated private manager endpoint.
    pub fn manager_endpoint(&self) -> &Url {
        &self.bootstrap.manager.private_endpoint
    }

    /// Returns the validated immutable local observation configuration.
    pub fn observation_configuration(&self) -> &NodeObservationConfiguration {
        &self.bootstrap.observation
    }

    /// Returns the exact immutable target-owned realization configuration.
    pub fn realization_targets(&self) -> &[ExecutionTarget] {
        &self.bootstrap.realization_targets
    }

    /// Returns the machine the node declares in place of its real hardware, if any.
    pub fn declared_machine(&self) -> Option<MachineDescription> {
        self.bootstrap
            .declared_machine_description()
            .expect("a validated folder declares only a valid machine")
    }

    /// Returns the stored declared machine, with the choices it was resolved from, if any.
    pub fn stored_declared_machine(&self) -> Option<&DeclaredMachine> {
        self.bootstrap.declared_machine.as_ref()
    }

    /// Returns the digest of the validated local observation configuration.
    pub fn observation_configuration_sha256(&self) -> &NodeObservationConfigurationDigest {
        &self.bootstrap.observation_configuration_sha256
    }

    /// Returns the site-local DER trust anchor.
    pub fn trust_anchor_der(&self) -> &[u8] {
        &self.trust_anchor
    }

    /// Returns the pinned manager DER leaf certificate.
    pub fn manager_certificate_der(&self) -> &[u8] {
        &self.manager_certificate
    }

    /// Returns the complete pinned manager leaf fingerprint.
    pub fn manager_certificate_sha256(&self) -> &CertificateFingerprint {
        &self.bootstrap.manager.certificate_sha256
    }

    /// Returns the complete worker leaf fingerprint.
    pub fn worker_certificate_sha256(&self) -> &CertificateFingerprint {
        &self.bootstrap.worker_certificate_sha256
    }

    /// Returns the worker DER leaf certificate.
    pub fn worker_certificate_der(&self) -> &[u8] {
        &self.worker_certificate
    }

    /// Returns the worker PKCS#8 private key for authenticated transport composition.
    pub fn worker_private_key_pkcs8_der(&self) -> &[u8] {
        &self.worker_private_key
    }

    /// Consumes the validated node folder into rustls worker identity parts.
    pub fn into_worker_identity(self) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
        (
            CertificateDer::from(self.worker_certificate),
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.worker_private_key)),
        )
    }
}

impl fmt::Debug for ValidatedNodeFolder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ValidatedNodeFolder")
            .field("bootstrap", &self.bootstrap)
            .field("trust_anchor", &"<DER certificate>")
            .field("manager_certificate", &"<DER certificate>")
            .field("worker_certificate", &"<DER certificate>")
            .field("worker_private_key", &"<redacted>")
            .finish()
    }
}

/// PKCS#8 key bytes whose diagnostics are always redacted.
pub struct SecretPkcs8Key(Vec<u8>);

impl SecretPkcs8Key {
    /// Wraps DER PKCS#8 bytes for owner-only artifact creation.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl fmt::Debug for SecretPkcs8Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretPkcs8Key(<redacted>)")
    }
}

/// Writes a new private-key artifact without ever replacing an existing identity.
pub(crate) fn write_owner_only_pkcs8(path: &Path, key: SecretPkcs8Key) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(&key.0)?;
    dotcore_domain::fs_sync::sync(&file)
}

/// Builds the canonical X.509 URI claim for one exact site and SiteNode.
pub fn certificate_identity_uri(
    site_id: &str,
    node_id: &str,
) -> Result<String, NodeFolderContractError> {
    validate_identifier("site", site_id)?;
    validate_identifier("node", node_id)?;
    Ok(format!("{IDENTITY_URI_PREFIX}{site_id}:node:{node_id}"))
}

/// A worker bootstrap declaration is structurally unsafe or invalid.
#[derive(Debug, thiserror::Error)]
pub enum NodeFolderContractError {
    /// A stable site or node identifier violates the repository identifier rule.
    #[error("node folder {identity} ID must match [a-z][a-z0-9-]{{0,62}}")]
    InvalidIdentifier { identity: &'static str },

    /// The manager endpoint is not a narrow, absolute HTTPS authority.
    #[error(
        "node folder manager endpoint must be an HTTPS URL with an explicit port and no credentials, path, query, or fragment"
    )]
    PrivateManagerEndpoint,

    /// The manager endpoint could not be parsed as a URL.
    #[error("node folder manager endpoint is not a valid URL")]
    ManagerEndpoint(#[source] url::ParseError),

    /// A certificate pin is not a complete canonical SHA-256 digest.
    #[error(
        "node folder manager certificate fingerprint must be 64 lowercase hexadecimal characters"
    )]
    CertificateFingerprint,

    /// The node-local observation projection is malformed.
    #[error("node folder observation configuration is invalid")]
    ObservationConfiguration(#[from] NodeObservationConfigurationError),

    /// The target-owned realization projection is absent or disagrees with observation targets.
    #[error("node folder realization target configuration is invalid")]
    RealizationTargets,

    /// The observation projection differs from its declared digest.
    #[error("node folder observation configuration does not match its declared digest")]
    ObservationConfigurationDigest,

    /// The private bind address is the wildcard address or has no port.
    #[error(
        "node folder private bind address must be a specific IPv4 address or host name with a \
         non-zero port"
    )]
    PrivateBindAddress,

    /// The declared machine has no hardware description.
    #[error("node folder declares a machine without a description")]
    MissingMachineDescription,

    /// The declared machine's hardware description breaks a description rule.
    #[error("node folder declares an invalid machine")]
    DeclaredMachine(#[source] MachineError),
}

/// Node folder validation failed before process startup.
#[derive(Debug, thiserror::Error)]
pub enum NodeFolderValidationError {
    /// A stale credential path or manager-only signing key was found with a node folder.
    #[error("node folder has an obsolete or mixed local node identity layout")]
    ObsoleteIdentityLayout,

    /// The shared local identity directory is missing, symlinked, or accessible to others.
    #[error("node folder node trust must be an owner-only real directory")]
    NodeTrustDirectory,

    /// A fixed artifact could not be read.
    #[error("node folder {artifact} could not be read")]
    ReadArtifact {
        artifact: &'static str,
        #[source]
        source: io::Error,
    },

    /// The non-secret manifest is not valid restricted bootstrap YAML.
    #[error("node folder manifest is invalid")]
    Manifest(#[source] dotcore_proto::yaml::RestrictedYamlError),

    /// The parsed bootstrap declaration violates its contract.
    #[error("node folder bootstrap is invalid")]
    InvalidBootstrap(#[source] NodeFolderContractError),

    /// A manifest identity differs from process-local expectation.
    #[error("node folder {identity} identity does not match the declared process identity")]
    IdentityMismatch { identity: &'static str },

    /// A certificate is not a single valid DER X.509 certificate.
    #[error("node folder {role} certificate is not valid DER X.509")]
    CertificateParse { role: &'static str },

    /// A certificate is not valid for its expected chain, usage, or time.
    #[error("node folder {role} certificate is not trusted for its required use")]
    CertificateTrust { role: &'static str },

    /// A leaf's Dotcore identity claim differs from its expected site or node.
    #[error("node folder {role} certificate has an invalid site or node identity claim")]
    CertificateIdentity { role: &'static str },

    /// The manager leaf differs from the complete pinned fingerprint.
    #[error("node folder manager certificate does not match its pinned fingerprint")]
    ManagerFingerprint,

    /// The worker leaf differs from the complete pinned fingerprint.
    #[error("node folder worker certificate does not match its pinned fingerprint")]
    WorkerFingerprint,

    /// A node trust artifact is not a regular file.
    #[error("node folder node trust artifact {artifact} must be a regular file")]
    NodeArtifactNotRegular { artifact: &'static str },

    /// A node trust artifact is accessible to group or other users.
    #[error("node folder node trust artifact {artifact} must be owner-only (mode {mode:o})")]
    NodeArtifactPermissions { artifact: &'static str, mode: u32 },

    /// The private-key artifact is not a regular owner-only file.
    #[error("node folder private key must be a regular file")]
    PrivateKeyNotRegular,

    /// Group or other users have access to the private key.
    #[error(
        "node folder private key permissions must deny all group and other access, got mode {mode:o}"
    )]
    PrivateKeyPermissions { mode: u32 },

    /// The private-key artifact is not parseable PKCS#8 supported by rustls.
    #[error("node folder private key is not valid supported PKCS#8")]
    PrivateKeyParse,

    /// The worker private key does not correspond to its leaf certificate.
    #[error("node folder private key does not match the worker certificate")]
    KeyMismatch,
}

impl NodeFolderValidationError {
    /// Projects a credential-safe transport diagnostic with no secret or identity detail.
    pub fn safe_transport_message(&self) -> &'static str {
        "worker authentication failed"
    }
}

/// Reads and contract-validates a node folder's bootstrap manifest without checking its
/// certificates, for operations that run on a folder whose identity is no longer usable.
pub fn read_bootstrap(root: &Path) -> Result<NodeFolderBootstrap, NodeFolderValidationError> {
    let bootstrap = load_bootstrap(root)?;
    bootstrap
        .validate()
        .map_err(NodeFolderValidationError::InvalidBootstrap)?;
    Ok(bootstrap)
}

fn load_bootstrap(root: &Path) -> Result<NodeFolderBootstrap, NodeFolderValidationError> {
    let contents = fs::read_to_string(root.join(WORKER_MANIFEST_FILE)).map_err(|source| {
        NodeFolderValidationError::ReadArtifact {
            artifact: WORKER_MANIFEST_FILE,
            source,
        }
    })?;
    dotcore_proto::yaml::from_str(&contents).map_err(NodeFolderValidationError::Manifest)
}

fn reject_invalid_worker_identity_layout(root: &Path) -> Result<(), NodeFolderValidationError> {
    let trust_root = root.join(crate::manager_trust::SITE_TRUST_DIRECTORY);
    for (directory, name) in [
        (root, "site-ca.der"),
        (root, "worker.der"),
        (root, "worker-key.pk8"),
        (root, crate::manager_trust::SITE_CA_PRIVATE_KEY_FILE),
        (
            trust_root.as_path(),
            crate::manager_trust::SITE_CA_PRIVATE_KEY_FILE,
        ),
    ] {
        match fs::symlink_metadata(directory.join(name)) {
            Ok(_) => return Err(NodeFolderValidationError::ObsoleteIdentityLayout),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(NodeFolderValidationError::ReadArtifact {
                    artifact: name,
                    source,
                });
            }
        }
    }
    Ok(())
}

fn map_node_identity_error(error: LocalNodeIdentityError) -> NodeFolderValidationError {
    match error {
        LocalNodeIdentityError::Directory => NodeFolderValidationError::NodeTrustDirectory,
        LocalNodeIdentityError::ReadArtifact { artifact, source } => {
            NodeFolderValidationError::ReadArtifact { artifact, source }
        }
        LocalNodeIdentityError::ArtifactNotRegular { artifact }
            if artifact == NODE_PRIVATE_KEY_FILE =>
        {
            NodeFolderValidationError::PrivateKeyNotRegular
        }
        LocalNodeIdentityError::ArtifactNotRegular { artifact } => {
            NodeFolderValidationError::NodeArtifactNotRegular { artifact }
        }
        LocalNodeIdentityError::ArtifactPermissions { mode, artifact }
            if artifact == NODE_PRIVATE_KEY_FILE =>
        {
            NodeFolderValidationError::PrivateKeyPermissions { mode }
        }
        LocalNodeIdentityError::ArtifactPermissions { mode, artifact } => {
            NodeFolderValidationError::NodeArtifactPermissions { artifact, mode }
        }
        LocalNodeIdentityError::CertificateParse => {
            NodeFolderValidationError::CertificateParse { role: "worker" }
        }
        LocalNodeIdentityError::CertificateIdentity => {
            NodeFolderValidationError::CertificateIdentity { role: "worker" }
        }
        LocalNodeIdentityError::CertificateTrust => {
            NodeFolderValidationError::CertificateTrust { role: "worker" }
        }
        LocalNodeIdentityError::PrivateKeyParse => NodeFolderValidationError::PrivateKeyParse,
        LocalNodeIdentityError::KeyMismatch => NodeFolderValidationError::KeyMismatch,
    }
}

fn read_artifact(
    root: &Path,
    artifact: &'static str,
) -> Result<Vec<u8>, NodeFolderValidationError> {
    fs::read(root.join(artifact))
        .map_err(|source| NodeFolderValidationError::ReadArtifact { artifact, source })
}

fn validate_certificate_parse(
    role: &'static str,
    certificate_der: &[u8],
) -> Result<(), NodeFolderValidationError> {
    let (remainder, _) = x509_parser::certificate::X509Certificate::from_der(certificate_der)
        .map_err(|_| NodeFolderValidationError::CertificateParse { role })?;
    if !remainder.is_empty() {
        return Err(NodeFolderValidationError::CertificateParse { role });
    }
    Ok(())
}

fn validate_fingerprints(
    bootstrap: &NodeFolderBootstrap,
    manager_certificate: &[u8],
    worker_certificate: &[u8],
) -> Result<(), NodeFolderValidationError> {
    if CertificateFingerprint::for_certificate(manager_certificate)
        != bootstrap.manager.certificate_sha256
    {
        return Err(NodeFolderValidationError::ManagerFingerprint);
    }
    if CertificateFingerprint::for_certificate(worker_certificate)
        != bootstrap.worker_certificate_sha256
    {
        return Err(NodeFolderValidationError::WorkerFingerprint);
    }
    Ok(())
}

fn validate_certificate_claim(
    role: &'static str,
    certificate_der: &[u8],
    site_id: &str,
    node_id: &str,
) -> Result<(), NodeFolderValidationError> {
    let (_, certificate) = x509_parser::certificate::X509Certificate::from_der(certificate_der)
        .map_err(|_| NodeFolderValidationError::CertificateParse { role })?;
    let expected = certificate_identity_uri(site_id, node_id)
        .map_err(NodeFolderValidationError::InvalidBootstrap)?;
    let identities = certificate
        .subject_alternative_name()
        .map_err(|_| NodeFolderValidationError::CertificateParse { role })?
        .into_iter()
        .flat_map(|extension| extension.value.general_names.iter())
        .filter_map(|name| match name {
            GeneralName::URI(uri) if uri.starts_with(IDENTITY_URI_PREFIX) => Some(*uri),
            _ => None,
        })
        .collect::<Vec<_>>();
    if identities.as_slice() != [expected.as_str()] {
        return Err(NodeFolderValidationError::CertificateIdentity { role });
    }
    Ok(())
}

/// Checks that the manager leaf chains to the site trust anchor for server authentication. The
/// manager endpoint host is not matched: the worker may reach the manager at any address.
fn validate_trust(
    trust_anchor: &[u8],
    manager_certificate: &[u8],
) -> Result<(), NodeFolderValidationError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(trust_anchor.to_vec()))
        .map_err(|_| NodeFolderValidationError::CertificateTrust {
            role: "trust anchor",
        })?;
    verify_site_server_chain(
        &CertificateDer::from(manager_certificate),
        &[],
        &roots,
        &rustls::crypto::ring::default_provider(),
        UnixTime::now(),
    )
    .map_err(|_| NodeFolderValidationError::CertificateTrust { role: "manager" })
}

fn validate_identifier(identity: &'static str, value: &str) -> Result<(), NodeFolderContractError> {
    if !valid_id(value) {
        return Err(NodeFolderContractError::InvalidIdentifier { identity });
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    use fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    use dotcore_proto::{
        dotcore::{
            catalog::v1::OciPlatform,
            node::v1::{
                DeclaredMachine, NodeExecutionTarget, NodeObservationConfiguration,
                node_execution_target,
            },
            observation::v1::RuntimeKind,
            site::v1::{
                ExecutionTarget, ModelRuntimeBinding, SimulatedRuntimeAdapter, execution_target,
                model_runtime_binding,
            },
        },
        google::protobuf::Empty,
    };
    use rcgen::{
        BasicConstraints, Certificate, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer,
        KeyPair, KeyUsagePurpose, SanType,
    };
    use tempfile::TempDir;
    use time::{Duration, OffsetDateTime};

    use super::{
        CertificateFingerprint, MANAGER_CERTIFICATE_FILE, NODE_CERTIFICATE_FILE,
        NODE_PRIVATE_KEY_FILE, NodeFolderBootstrap, NodeFolderExpectation, NodeFolderManager,
        NodeFolderValidationError, NodeFolderValidator, SecretPkcs8Key, TRUST_ANCHOR_FILE,
        WORKER_MANIFEST_FILE, certificate_identity_uri, write_owner_only_pkcs8,
    };
    use crate::manager_trust::{SITE_CA_PRIVATE_KEY_FILE, SITE_TRUST_DIRECTORY};

    #[test]
    fn validates_complete_node_folder_and_keeps_secret_out_of_serialized_contract_and_debug() {
        let fixture = NodeFolderFixture::new();

        let validated = fixture.validate().expect("valid node folder");

        assert_eq!(validated.site_id(), "site-one");
        assert_eq!(validated.worker_node_id(), "worker-one");
        assert_eq!(validated.manager_node_id(), "manager-one");
        assert_eq!(
            validated.manager_endpoint().as_str(),
            "https://localhost:9443/"
        );
        assert_eq!(
            validated.worker_certificate_der(),
            fixture.worker_certificate.as_slice()
        );
        assert_eq!(
            validated.manager_certificate_der(),
            fixture.manager_certificate.as_slice()
        );
        assert_eq!(
            validated.trust_anchor_der(),
            fixture.trust_anchor.as_slice()
        );
        let yaml = fs::read_to_string(fixture.root.path().join(WORKER_MANIFEST_FILE))
            .expect("node folder manifest");
        assert!(!yaml.contains("privateKey"));
        assert!(!yaml.contains("private-key"));
        assert!(!format!("{validated:?}").contains(&fixture.private_key_hex_prefix()));
    }

    #[test]
    fn rejects_site_signing_key_in_node_folder() {
        for location in ["node folder root", "node trust"] {
            let fixture = NodeFolderFixture::new();
            let directory = if location == "node trust" {
                fixture.root.path().join(SITE_TRUST_DIRECTORY)
            } else {
                fixture.root.path().to_path_buf()
            };
            let path = directory.join(SITE_CA_PRIVATE_KEY_FILE);
            fs::write(&path, b"unexpected signing key").expect("place signing key");

            assert!(
                matches!(
                    fixture.validate(),
                    Err(NodeFolderValidationError::ObsoleteIdentityLayout)
                ),
                "accepted manager-only signing key at {location}"
            );
            assert_eq!(
                fs::read(&path).expect("signing key remains"),
                b"unexpected signing key"
            );
        }
    }

    #[test]
    fn rejects_each_declared_identity_mismatch() {
        for expectation in [
            NodeFolderExpectation::new("site-two", "worker-one", "manager-one"),
            NodeFolderExpectation::new("site-one", "worker-two", "manager-one"),
            NodeFolderExpectation::new("site-one", "worker-one", "manager-two"),
        ] {
            let fixture = NodeFolderFixture::new();
            let error = NodeFolderValidator::new(expectation)
                .validate(fixture.root.path())
                .expect_err("identity mismatch");

            assert!(matches!(
                error,
                NodeFolderValidationError::IdentityMismatch { .. }
            ));
            assert_eq!(
                error.safe_transport_message(),
                "worker authentication failed"
            );
        }
    }

    #[test]
    fn rejects_observation_configuration_digest_mismatch() {
        let fixture = NodeFolderFixture::new();
        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.observation_configuration_sha256 =
                super::NodeObservationConfigurationDigest::parse("0".repeat(64))
                    .expect("different canonical digest");
        });

        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::InvalidBootstrap(
                super::NodeFolderContractError::ObservationConfigurationDigest
            ))
        ));
    }

    #[test]
    fn manifests_without_realization_targets_or_bind_address_are_rejected() {
        for field in ["realizationTargets:", "privateBindAddress:"] {
            let fixture = NodeFolderFixture::new();
            let path = fixture.root.path().join(WORKER_MANIFEST_FILE);
            let manifest = fs::read_to_string(&path).expect("manifest");
            let mut skipping = false;
            let mut kept = Vec::new();
            for line in manifest.lines() {
                if line.starts_with(field) {
                    skipping = true;
                    continue;
                }
                if skipping && (line.starts_with(char::is_whitespace) || line.starts_with("- ")) {
                    continue;
                }
                skipping = false;
                kept.push(line);
            }
            fs::write(&path, format!("{}\n", kept.join("\n"))).expect("manifest without field");

            assert!(
                matches!(
                    fixture.validate(),
                    Err(NodeFolderValidationError::Manifest(_))
                ),
                "{field}"
            );
        }
    }

    #[test]
    fn private_bind_address_must_be_a_specific_host_with_a_port() {
        for address in ["0.0.0.0:7443", "10.0.0.7:0"] {
            let fixture = NodeFolderFixture::new();
            fixture.rewrite_manifest(|bootstrap| {
                bootstrap.private_bind_address = address.parse().expect("bind address");
            });

            assert!(
                matches!(
                    fixture.validate(),
                    Err(NodeFolderValidationError::InvalidBootstrap(
                        super::NodeFolderContractError::PrivateBindAddress
                    ))
                ),
                "{address}"
            );
        }
        for address in [
            "'[::1]:7443'",
            "desk_app:7443",
            "LocalHost:7443",
            "localhost.:7443",
        ] {
            let fixture = NodeFolderFixture::new();
            fixture.rewrite_manifest_text(|text| {
                text.replace(
                    "privateBindAddress: 10.0.0.7:7443",
                    &format!("privateBindAddress: {address}"),
                )
            });

            assert!(
                matches!(
                    fixture.validate(),
                    Err(NodeFolderValidationError::Manifest(_))
                ),
                "{address}"
            );
        }
        let fixture = NodeFolderFixture::new();
        assert_eq!(
            fixture
                .validate()
                .expect("valid node folder")
                .private_bind_address()
                .to_string(),
            "10.0.0.7:7443"
        );
    }

    #[test]
    fn private_bind_address_keeps_a_host_name() {
        let fixture = NodeFolderFixture::new();
        fixture.rewrite_manifest_text(|text| {
            text.replace(
                "privateBindAddress: 10.0.0.7:7443",
                "privateBindAddress: localhost:7443",
            )
        });

        assert_eq!(
            fixture
                .validate()
                .expect("valid node folder")
                .private_bind_address()
                .to_string(),
            "localhost:7443"
        );
    }

    #[test]
    fn a_folder_declares_only_a_valid_machine() {
        let fixture = NodeFolderFixture::new();
        assert_eq!(
            fixture
                .validate()
                .expect("valid node folder")
                .declared_machine(),
            None
        );

        let gb10 = dotcore_node_hardware::MachineDescription::named("gb10").expect("named machine");
        let declared = DeclaredMachine {
            choices: vec!["gb10".to_owned()],
            description: Some(gb10.to_stored()),
        };
        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.declared_machine = Some(declared.clone());
        });
        let folder = fixture.validate().expect("folder declaring a machine");
        assert_eq!(folder.stored_declared_machine(), Some(&declared));
        assert_eq!(folder.declared_machine(), Some(gb10));

        fixture.rewrite_manifest(|bootstrap| {
            let mut invalid = declared.clone();
            if let Some(description) = invalid.description.as_mut() {
                description.platform = None;
            }
            bootstrap.declared_machine = Some(invalid);
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::InvalidBootstrap(
                super::NodeFolderContractError::DeclaredMachine(_)
            ))
        ));

        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.declared_machine = Some(DeclaredMachine {
                choices: vec!["gb10".to_owned()],
                description: None,
            });
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::InvalidBootstrap(
                super::NodeFolderContractError::MissingMachineDescription
            ))
        ));
    }

    #[test]
    fn rejects_duplicate_realization_target_identities() {
        let fixture = NodeFolderFixture::new();
        fixture.rewrite_manifest(|bootstrap| {
            let targets = &mut bootstrap.realization_targets;
            targets.push(targets[0].clone());
        });

        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::InvalidBootstrap(
                super::NodeFolderContractError::RealizationTargets
            ))
        ));
    }
    #[test]
    fn rejects_certificate_claim_and_manager_fingerprint_mismatches() {
        for (site_id, node_id) in [("site-two", "worker-one"), ("site-one", "worker-two")] {
            let fixture = NodeFolderFixture::new();
            fixture.replace_worker_certificate(site_id, node_id);
            assert!(matches!(
                fixture.validate(),
                Err(NodeFolderValidationError::CertificateIdentity { role: "worker", .. })
            ));
        }

        let fixture = NodeFolderFixture::new();
        fixture.replace_manager_certificate("site-one", "manager-two");
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateIdentity {
                role: "manager",
                ..
            })
        ));

        let fixture = NodeFolderFixture::new();
        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.manager.certificate_sha256 = CertificateFingerprint::from_bytes([0x55; 32]);
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::ManagerFingerprint)
        ));

        let fixture = NodeFolderFixture::new();
        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.worker_certificate_sha256 = CertificateFingerprint::from_bytes([0x66; 32]);
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::WorkerFingerprint)
        ));
    }

    #[test]
    fn rejects_yaml_features_outside_the_restricted_bootstrap_contract() {
        let fixture = NodeFolderFixture::new();
        fs::write(
            fixture.root.path().join(WORKER_MANIFEST_FILE),
            "siteId: &site site-one\nworkerNodeId: *site\n",
        )
        .expect("replace manifest");

        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_foreign_trust_chain_and_malformed_certificate() {
        let fixture = NodeFolderFixture::new();
        let foreign = TestAuthority::new();
        fs::write(
            fixture
                .root
                .path()
                .join(SITE_TRUST_DIRECTORY)
                .join(TRUST_ANCHOR_FILE),
            foreign.certificate.der(),
        )
        .expect("replace trust anchor");
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateTrust { .. })
        ));

        let fixture = NodeFolderFixture::new();
        fs::write(
            fixture.root.path().join(MANAGER_CERTIFICATE_FILE),
            b"not DER",
        )
        .expect("replace manager certificate");
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateParse {
                role: "manager",
                ..
            })
        ));
    }

    #[test]
    fn validates_a_manager_endpoint_host_absent_from_the_manager_certificate() {
        let fixture = NodeFolderFixture::new();
        fixture.replace_manager_certificate_with(|parameters| {
            parameters
                .subject_alt_names
                .retain(|name| matches!(name, SanType::URI(_)));
        });
        fixture.rewrite_manifest(|bootstrap| {
            bootstrap.manager = NodeFolderManager::new(
                "manager-one",
                "https://100.64.0.10:9443",
                bootstrap.manager.certificate_sha256.clone(),
            )
            .expect("manager bootstrap");
        });

        let validated = fixture
            .validate()
            .expect("address-free manager leaf validates");
        assert_eq!(
            validated.manager_endpoint().as_str(),
            "https://100.64.0.10:9443/"
        );
    }

    #[test]
    fn rejects_wrong_usage_and_expiration() {
        let fixture = NodeFolderFixture::new();
        fixture.replace_manager_certificate_with(|parameters| {
            parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateTrust {
                role: "manager",
                ..
            })
        ));

        let fixture = NodeFolderFixture::new();
        fixture.replace_worker_certificate_with(|parameters| {
            parameters.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateTrust { role: "worker", .. })
        ));

        let fixture = NodeFolderFixture::new();
        fixture.replace_manager_certificate_with(|parameters| {
            parameters.not_before = OffsetDateTime::now_utc() - Duration::hours(2);
            parameters.not_after = OffsetDateTime::now_utc() - Duration::hours(1);
        });
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::CertificateTrust {
                role: "manager",
                ..
            })
        ));
    }

    #[test]
    fn rejects_private_key_certificate_mismatch_without_disclosing_key() {
        let fixture = NodeFolderFixture::new();
        let unrelated = KeyPair::generate().expect("unrelated key").serialize_der();
        fs::write(
            fixture
                .root
                .path()
                .join(SITE_TRUST_DIRECTORY)
                .join(NODE_PRIVATE_KEY_FILE),
            &unrelated,
        )
        .expect("replace worker key");

        let error = fixture.validate().expect_err("key mismatch");

        assert!(matches!(error, NodeFolderValidationError::KeyMismatch));
        assert!(!format!("{error:?}").contains(&hex_prefix(&unrelated)));
        assert!(!error.to_string().contains(&hex_prefix(&unrelated)));
    }

    #[test]
    fn rejects_malformed_pkcs8_without_disclosing_artifact_bytes() {
        let fixture = NodeFolderFixture::new();
        let malformed = b"not a private key";
        fs::write(
            fixture
                .root
                .path()
                .join(SITE_TRUST_DIRECTORY)
                .join(NODE_PRIVATE_KEY_FILE),
            malformed,
        )
        .expect("replace worker key");

        let error = fixture.validate().expect_err("malformed key");

        assert!(matches!(error, NodeFolderValidationError::PrivateKeyParse));
        assert!(!format!("{error:?}").contains("not a private key"));
        assert!(!error.to_string().contains("not a private key"));
    }

    #[test]
    fn rejects_non_owner_only_private_key_permissions() {
        let fixture = NodeFolderFixture::new();
        let path = fixture
            .root
            .path()
            .join(SITE_TRUST_DIRECTORY)
            .join(NODE_PRIVATE_KEY_FILE);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("loosen key mode");

        let error = fixture.validate().expect_err("unsafe key permissions");

        assert!(matches!(
            error,
            NodeFolderValidationError::PrivateKeyPermissions { mode: 0o640 }
        ));
    }

    #[test]
    fn rejects_shared_trust_anchor_accessible_to_other_users() {
        let fixture = NodeFolderFixture::new();
        let anchor = fixture
            .root
            .path()
            .join(SITE_TRUST_DIRECTORY)
            .join(TRUST_ANCHOR_FILE);
        fs::set_permissions(anchor, fs::Permissions::from_mode(0o644))
            .expect("loosen trust anchor mode");
        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::NodeArtifactPermissions {
                artifact: TRUST_ANCHOR_FILE,
                mode: 0o644
            })
        ));
    }

    #[test]
    fn private_key_writer_uses_create_new_and_owner_only_mode() {
        let root = dotcore_domain::test_support::canonical_tempdir().expect("temporary directory");
        let path = root.path().join("identity.pk8");
        let key = KeyPair::generate().expect("key").serialize_der();

        write_owner_only_pkcs8(&path, SecretPkcs8Key::new(key.clone())).expect("write key");

        assert_eq!(fs::read(&path).expect("written key"), key);
        assert_eq!(
            fs::metadata(&path)
                .expect("key metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let error = write_owner_only_pkcs8(
            &path,
            SecretPkcs8Key::new(
                KeyPair::generate()
                    .expect("replacement key")
                    .serialize_der(),
            ),
        )
        .expect_err("must not replace a key");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn private_key_custody_refuses_symlinks_for_creation_and_validation() {
        let root = dotcore_domain::test_support::canonical_tempdir().expect("temporary directory");
        let target = root.path().join("target.pk8");
        let link = root.path().join("identity.pk8");
        fs::write(&target, b"retained").expect("target contents");
        symlink(&target, &link).expect("private-key symlink");

        let error = write_owner_only_pkcs8(
            &link,
            SecretPkcs8Key::new(KeyPair::generate().expect("key").serialize_der()),
        )
        .expect_err("must not follow a symlink");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read(&target).expect("target after refusal"),
            b"retained"
        );

        let fixture = NodeFolderFixture::new();
        let key_path = fixture
            .root
            .path()
            .join(SITE_TRUST_DIRECTORY)
            .join(NODE_PRIVATE_KEY_FILE);
        let target_path = fixture.root.path().join("moved-worker-key.pk8");
        fs::rename(&key_path, &target_path).expect("move worker key");
        symlink(&target_path, &key_path).expect("worker-key symlink");

        assert!(matches!(
            fixture.validate(),
            Err(NodeFolderValidationError::PrivateKeyNotRegular)
        ));
    }

    struct NodeFolderFixture {
        root: TempDir,
        authority: TestAuthority,
        trust_anchor: Vec<u8>,
        manager_certificate: Vec<u8>,
        worker_certificate: Vec<u8>,
        worker_key: Vec<u8>,
    }

    impl NodeFolderFixture {
        fn new() -> Self {
            let root =
                dotcore_domain::test_support::canonical_tempdir().expect("temporary node folder");
            let authority = TestAuthority::new();
            let (manager_certificate, _) = authority.issue("site-one", "manager-one", true);
            let (worker_certificate, worker_key) = authority.issue("site-one", "worker-one", false);
            let trust_anchor = authority.certificate.der().to_vec();
            let bootstrap = NodeFolderBootstrap::new(
                "site-one",
                "worker-one",
                CertificateFingerprint::for_certificate(&worker_certificate),
                NodeFolderManager::new(
                    "manager-one",
                    "https://localhost:9443",
                    CertificateFingerprint::for_certificate(&manager_certificate),
                )
                .expect("manager bootstrap"),
                observation_configuration(),
                vec![realization_target()],
                "10.0.0.7:7443".parse().expect("private bind address"),
            )
            .expect("worker bootstrap");
            fs::write(
                root.path().join(WORKER_MANIFEST_FILE),
                dotcore_proto::yaml::to_string(&bootstrap).expect("serialize bootstrap"),
            )
            .expect("write bootstrap");
            let node_trust = root.path().join(SITE_TRUST_DIRECTORY);
            fs::create_dir(&node_trust).expect("node trust directory");
            fs::set_permissions(&node_trust, fs::Permissions::from_mode(0o700))
                .expect("owner-only node trust");
            fs::write(node_trust.join(TRUST_ANCHOR_FILE), &trust_anchor)
                .expect("write trust anchor");
            fs::write(
                root.path().join(MANAGER_CERTIFICATE_FILE),
                &manager_certificate,
            )
            .expect("write manager certificate");
            fs::write(node_trust.join(NODE_CERTIFICATE_FILE), &worker_certificate)
                .expect("write worker certificate");
            write_owner_only_pkcs8(
                &node_trust.join(NODE_PRIVATE_KEY_FILE),
                SecretPkcs8Key::new(worker_key.clone()),
            )
            .expect("write worker key");
            for name in [TRUST_ANCHOR_FILE, NODE_CERTIFICATE_FILE] {
                fs::set_permissions(node_trust.join(name), fs::Permissions::from_mode(0o600))
                    .expect("owner-only node artifact");
            }
            Self {
                root,
                authority,
                trust_anchor,
                manager_certificate,
                worker_certificate,
                worker_key,
            }
        }

        fn validate(&self) -> Result<super::ValidatedNodeFolder, NodeFolderValidationError> {
            NodeFolderValidator::new(NodeFolderExpectation::new(
                "site-one",
                "worker-one",
                "manager-one",
            ))
            .validate(self.root.path())
        }

        fn replace_worker_certificate(&self, site_id: &str, node_id: &str) {
            let (certificate, _) = self.authority.issue(site_id, node_id, false);
            fs::write(
                self.root
                    .path()
                    .join(SITE_TRUST_DIRECTORY)
                    .join(NODE_CERTIFICATE_FILE),
                &certificate,
            )
            .expect("replace worker certificate");
            self.rewrite_manifest(|bootstrap| {
                bootstrap.worker_certificate_sha256 =
                    CertificateFingerprint::for_certificate(&certificate);
            });
        }

        fn replace_manager_certificate(&self, site_id: &str, node_id: &str) {
            let (certificate, _) = self.authority.issue(site_id, node_id, true);
            self.install_manager_certificate(&certificate);
        }

        fn replace_manager_certificate_with(&self, update: impl FnOnce(&mut CertificateParams)) {
            let (certificate, _) =
                self.authority
                    .issue_with("site-one", "manager-one", true, update);
            self.install_manager_certificate(&certificate);
        }

        fn replace_worker_certificate_with(&self, update: impl FnOnce(&mut CertificateParams)) {
            let (certificate, _) =
                self.authority
                    .issue_with("site-one", "worker-one", false, update);
            fs::write(
                self.root
                    .path()
                    .join(SITE_TRUST_DIRECTORY)
                    .join(NODE_CERTIFICATE_FILE),
                &certificate,
            )
            .expect("replace worker certificate");
            self.rewrite_manifest(|bootstrap| {
                bootstrap.worker_certificate_sha256 =
                    CertificateFingerprint::for_certificate(&certificate);
            });
        }

        fn install_manager_certificate(&self, certificate: &[u8]) {
            fs::write(self.root.path().join(MANAGER_CERTIFICATE_FILE), certificate)
                .expect("replace manager certificate");
            self.rewrite_manifest(|bootstrap| {
                bootstrap.manager.certificate_sha256 =
                    CertificateFingerprint::for_certificate(certificate);
            });
        }

        fn rewrite_manifest(&self, update: impl FnOnce(&mut NodeFolderBootstrap)) {
            let path = self.root.path().join(WORKER_MANIFEST_FILE);
            let mut bootstrap: NodeFolderBootstrap =
                dotcore_proto::yaml::from_str(&fs::read_to_string(&path).expect("read bootstrap"))
                    .expect("parse bootstrap");
            update(&mut bootstrap);
            fs::write(
                path,
                dotcore_proto::yaml::to_string(&bootstrap).expect("serialize bootstrap"),
            )
            .expect("rewrite bootstrap");
        }

        fn rewrite_manifest_text(&self, update: impl FnOnce(&str) -> String) {
            let path = self.root.path().join(WORKER_MANIFEST_FILE);
            let text = fs::read_to_string(&path).expect("read bootstrap");
            let updated = update(&text);
            assert_ne!(updated, text, "the manifest text changed");
            fs::write(path, updated).expect("rewrite bootstrap");
        }

        fn private_key_hex_prefix(&self) -> String {
            hex_prefix(&self.worker_key)
        }
    }

    fn realization_target() -> ExecutionTarget {
        ExecutionTarget {
            execution_target_id: "model-target".to_owned(),
            display_name: "model-target".to_owned(),
            site_node_id: "worker-one".to_owned(),
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: "amd64".to_owned(),
                variant: None,
            }),
            accelerator_device_ids: Vec::new(),
            runtime_binding: Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding {
                    adapter: Some(model_runtime_binding::Adapter::SimulatedRuntime(
                        SimulatedRuntimeAdapter::default(),
                    )),
                },
            )),
        }
    }

    fn observation_configuration() -> NodeObservationConfiguration {
        NodeObservationConfiguration {
            schema_version: 1,
            interval_seconds: 5,
            execution_targets: vec![NodeExecutionTarget {
                execution_target_id: "model-target".to_owned(),
                kind: RuntimeKind::Model as i32,
                platform: Some(OciPlatform {
                    operating_system: "linux".to_owned(),
                    architecture: "amd64".to_owned(),
                    variant: None,
                }),
                implementation: Some(node_execution_target::Implementation::Simulated(Empty {})),
            }],
        }
    }

    struct TestAuthority {
        certificate: Certificate,
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

        fn issue(&self, site_id: &str, node_id: &str, manager: bool) -> (Vec<u8>, Vec<u8>) {
            self.issue_with(site_id, node_id, manager, |_| {})
        }

        fn issue_with(
            &self,
            site_id: &str,
            node_id: &str,
            manager: bool,
            update: impl FnOnce(&mut CertificateParams),
        ) -> (Vec<u8>, Vec<u8>) {
            let names = if manager {
                vec!["localhost".to_owned()]
            } else {
                Vec::new()
            };
            let mut parameters = CertificateParams::new(names).expect("leaf params");
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
            parameters.not_before = OffsetDateTime::now_utc() - Duration::hours(1);
            parameters.not_after = OffsetDateTime::now_utc() + Duration::hours(1);
            update(&mut parameters);
            let key = KeyPair::generate().expect("leaf key");
            let certificate = parameters
                .signed_by(&key, &self.issuer)
                .expect("leaf certificate");
            (certificate.der().to_vec(), key.serialize_der())
        }
    }

    fn hex_prefix(bytes: &[u8]) -> String {
        bytes
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}
