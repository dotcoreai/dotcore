//! Installs a joined node's folder: its `worker.yaml`, the pinned manager certificate, and its
//! own trust files with the private key it generated, in an owner-only directory that the joined
//! node's server later starts from.
//!
//! The folder is assembled and validated in a hidden staging directory next to its final place
//! and then moved there in one rename that never replaces an existing folder, so the node either
//! has a complete folder or none.
//!
//! A declared node joining the same site again reopens its intact folder instead: the folder is
//! checked against the join token and held in the node's custody, and only its `worker.yaml`
//! addresses and observation configuration are rewritten, in one atomic replacement, once the
//! manager recorded the rejoin.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use dotcore_domain::BindAddress;
use dotcore_proto::dotcore::node::v1::NodeObservationConfiguration;

use super::{
    JoinToken, NodeKey, NodeKeyError,
    node_key::{CertificateTime, certificate_time},
};
use crate::{
    atomic::{self, AtomicError, FileSystem, persist_bytes},
    manager_trust::SITE_TRUST_DIRECTORY,
    node::folder::{
        CertificateFingerprint, MANAGER_CERTIFICATE_FILE, NODE_CERTIFICATE_FILE,
        NODE_PRIVATE_KEY_FILE, NodeFolderBootstrap, NodeFolderContractError, NodeFolderExpectation,
        NodeFolderManager, NodeFolderValidationError, NodeFolderValidator, SecretPkcs8Key,
        TRUST_ANCHOR_FILE, ValidatedNodeFolder, WORKER_MANIFEST_FILE, WorkerNodeCustody,
        WorkerNodeCustodyError, read_bootstrap, write_owner_only_pkcs8,
    },
};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A declared node's intact folder opened to join the same site again. It was checked against
/// the join token before anything is sent, and it holds the node's custody so the node cannot
/// start until the rejoin ends.
pub struct RejoiningNodeFolder {
    root: PathBuf,
    folder: ValidatedNodeFolder,
    key: NodeKey,
    _custody: WorkerNodeCustody,
}

impl RejoiningNodeFolder {
    /// Opens the node folder at `root` to join `token`'s site again.
    ///
    /// Refuses a folder of another site, a folder whose node certificate has expired or is not
    /// valid yet, a folder of an earlier trust of the same site (another site CA or manager
    /// certificate than the token pins), and a folder whose node is running. Nothing is changed
    /// or sent.
    pub fn open(root: &Path, token: &JoinToken) -> Result<Self, NodeFolderError> {
        let bootstrap = read_bootstrap(root).map_err(NodeFolderError::Invalid)?;
        if bootstrap.site_id != token.site_id {
            return Err(NodeFolderError::OtherSite {
                root: root.to_owned(),
            });
        }
        let folder = NodeFolderValidator::validate_declared(root)
            .map_err(|error| invalid_folder(root, error))?;
        if folder.manager_node_id() != token.manager_node_id
            || *folder.manager_certificate_sha256() != token.manager_certificate_sha256
            || CertificateFingerprint::for_certificate(folder.trust_anchor_der())
                != token.site_ca_sha256
        {
            return Err(NodeFolderError::EarlierTrust {
                root: root.to_owned(),
            });
        }
        let custody = WorkerNodeCustody::try_acquire(root).map_err(|error| match error {
            WorkerNodeCustodyError::AlreadyHeld => NodeFolderError::NodeRunning {
                root: root.to_owned(),
            },
            WorkerNodeCustodyError::Storage { source } => NodeFolderError::Storage(source),
        })?;
        let key = NodeKey::from_pkcs8(folder.worker_private_key_pkcs8_der())
            .map_err(NodeFolderError::Key)?;
        Ok(Self {
            root: root.to_owned(),
            folder,
            key,
            _custody: custody,
        })
    }

    /// Returns the directory holding the node folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the SiteNode identity the folder holds.
    pub fn node_id(&self) -> &str {
        self.folder.worker_node_id()
    }

    /// Returns the port the node's private listener binds now.
    pub fn private_port(&self) -> u16 {
        self.folder.private_bind_address().port()
    }

    pub(super) fn folder(&self) -> &ValidatedNodeFolder {
        &self.folder
    }

    pub(super) fn key(&self) -> &NodeKey {
        &self.key
    }

    /// Returns the folder's bootstrap manifest with the manager endpoint, the private bind
    /// address, and the observation configuration replaced; everything else, the declared machine
    /// included, is kept as installed.
    pub(super) fn rejoined_bootstrap(
        &self,
        manager_private_endpoint: &str,
        private_bind_address: BindAddress,
        observation: NodeObservationConfiguration,
    ) -> Result<NodeFolderBootstrap, NodeFolderContractError> {
        let folder = &self.folder;
        let manager = NodeFolderManager::new(
            folder.manager_node_id(),
            manager_private_endpoint,
            folder.manager_certificate_sha256().clone(),
        )?;
        NodeFolderBootstrap::new(
            folder.site_id(),
            folder.worker_node_id(),
            folder.worker_certificate_sha256().clone(),
            manager,
            observation,
            folder.realization_targets().to_vec(),
            private_bind_address,
        )?
        .with_declared_machine(folder.stored_declared_machine().cloned())
    }
}

/// Replaces the `worker.yaml` of the node folder at `root` with `bootstrap` in one atomic rename.
/// A failure before the rename leaves the previous manifest in place.
pub(super) fn rewrite_bootstrap(
    filesystem: &dyn FileSystem,
    root: &Path,
    bootstrap: &NodeFolderBootstrap,
) -> Result<(), NodeFolderError> {
    let manifest = dotcore_proto::yaml::to_string(bootstrap)
        .map_err(|source| NodeFolderError::Storage(anyhow::Error::new(source)))?;
    let path = root.join(WORKER_MANIFEST_FILE);
    persist_bytes(filesystem, &path, manifest.as_bytes()).map_err(|source| {
        let uncertain = matches!(source, AtomicError::OutcomeUncertain(_));
        let source = anyhow::Error::new(source).context(format!("replace {}", path.display()));
        if uncertain {
            NodeFolderError::RewriteUncertain(source)
        } else {
            NodeFolderError::Storage(source)
        }
    })
}

/// Everything a joined node folder holds.
pub(super) struct NodeFolderContents {
    pub(super) bootstrap: NodeFolderBootstrap,
    pub(super) manager_certificate_der: Vec<u8>,
    pub(super) site_ca_der: Vec<u8>,
    pub(super) node_certificate_der: Vec<u8>,
    pub(super) private_key: SecretPkcs8Key,
}

/// Fails when anything already exists at `root`, so a join never touches an existing site or
/// node folder.
pub(super) fn reject_existing_folder(root: &Path) -> Result<(), NodeFolderError> {
    match fs::symlink_metadata(root) {
        Ok(_) => Err(NodeFolderError::Exists {
            root: root.to_owned(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(storage("inspect node folder", root, source)),
    }
}

/// Stages, validates, and moves the complete folder to `root`. Nothing is left behind on failure.
pub(super) fn install_node_folder(
    root: &Path,
    contents: NodeFolderContents,
) -> Result<ValidatedNodeFolder, NodeFolderError> {
    reject_existing_folder(root)?;
    let parent = root.parent().ok_or_else(|| NodeFolderError::NoParent {
        root: root.to_owned(),
    })?;
    fs::create_dir_all(parent)
        .map_err(|source| storage("create node folder parent", parent, source))?;
    let staged = staging_path(root)?;

    let result = stage(&staged, contents).and_then(|validated| {
        rename_noreplace(&staged, root).map_err(|source| {
            if source.kind() == io::ErrorKind::AlreadyExists {
                NodeFolderError::Exists {
                    root: root.to_owned(),
                }
            } else {
                storage("move node folder into place", root, source)
            }
        })?;
        sync_directory(parent)?;
        Ok(validated)
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&staged);
    }
    result
}

/// Removes an installed folder after the manager clearly refused to record the node.
pub(super) fn remove_node_folder(root: &Path) -> Result<(), NodeFolderError> {
    fs::remove_dir_all(root).map_err(|source| storage("remove node folder", root, source))
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] instead of replacing an
/// existing `to`.
#[cfg(target_os = "linux")]
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = c_paths(from, to)?;
    // SAFETY: both C strings are valid for the duration of the call and AT_FDCWD uses absolute or
    // process-relative paths without borrowing directory descriptors.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Renames `from` to `to`, failing with [`io::ErrorKind::AlreadyExists`] instead of replacing an
/// existing `to`.
#[cfg(target_os = "macos")]
pub(crate) fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = c_paths(from, to)?;
    // SAFETY: both C strings are valid for the duration of the call.
    let result = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// A node folder could not be installed, removed, or reopened to join again.
#[derive(Debug, thiserror::Error)]
pub enum NodeFolderError {
    #[error("{root} already exists; remove the existing folder or choose another one")]
    Exists { root: PathBuf },

    #[error(
        "the node folder at {root} belongs to another site; leave that site, then join this one \
         as a new node"
    )]
    OtherSite { root: PathBuf },

    #[error(
        "the node folder at {root} belongs to an earlier trust of this site; leave it, then join \
         as a new node"
    )]
    EarlierTrust { root: PathBuf },

    #[error("the node using the folder at {root} is running; stop it, then join again")]
    NodeRunning { root: PathBuf },

    #[error(
        "the node certificate in the folder at {root} has expired; remove the node from the \
         site, leave this machine's node folder, then join as a new node"
    )]
    CertificateExpired { root: PathBuf },

    #[error(
        "the node certificate in the folder at {root} is not valid yet; check the clocks of this \
         machine and the manager"
    )]
    CertificateNotYetValid { root: PathBuf },

    #[error("the node folder's key cannot be used: {0}")]
    Key(#[source] NodeKeyError),

    #[error("node folder path {root} has no parent directory")]
    NoParent { root: PathBuf },

    #[error("node folder could not be created as a valid node folder: {0}")]
    Invalid(#[source] NodeFolderValidationError),

    #[error("node folder storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("the node folder was updated, but the update may not be durable: {0}")]
    RewriteUncertain(#[source] anyhow::Error),
}

fn stage(
    staged: &Path,
    contents: NodeFolderContents,
) -> Result<ValidatedNodeFolder, NodeFolderError> {
    create_owner_directory(staged)?;
    let manifest = dotcore_proto::yaml::to_string(&contents.bootstrap)
        .map_err(|source| NodeFolderError::Storage(anyhow::Error::new(source)))?;
    write_owner_file(&staged.join(WORKER_MANIFEST_FILE), manifest.as_bytes())?;
    write_owner_file(
        &staged.join(MANAGER_CERTIFICATE_FILE),
        &contents.manager_certificate_der,
    )?;
    let trust = staged.join(SITE_TRUST_DIRECTORY);
    create_owner_directory(&trust)?;
    write_owner_file(&trust.join(TRUST_ANCHOR_FILE), &contents.site_ca_der)?;
    write_owner_file(
        &trust.join(NODE_CERTIFICATE_FILE),
        &contents.node_certificate_der,
    )?;
    let key_path = trust.join(NODE_PRIVATE_KEY_FILE);
    write_owner_only_pkcs8(&key_path, contents.private_key)
        .map_err(|source| storage("write node private key", &key_path, source))?;
    // Taking custody once creates the owner-only lock file the node holds while it runs.
    drop(WorkerNodeCustody::try_acquire(staged).map_err(|source| {
        NodeFolderError::Storage(anyhow::Error::new(source).context("create node custody lock"))
    })?);
    sync_directory(&trust)?;
    sync_directory(staged)?;

    NodeFolderValidator::new(NodeFolderExpectation::new(
        &contents.bootstrap.site_id,
        &contents.bootstrap.worker_node_id,
        &contents.bootstrap.manager.node_id,
    ))
    .validate(staged)
    .map_err(NodeFolderError::Invalid)
}

/// Classifies a failed validation of a folder to rejoin from: a node certificate outside its
/// validity period gets its own refusal, anything else is an invalid folder.
fn invalid_folder(root: &Path, error: NodeFolderValidationError) -> NodeFolderError {
    if !matches!(
        error,
        NodeFolderValidationError::CertificateTrust { role: "worker" }
    ) {
        return NodeFolderError::Invalid(error);
    }
    let certificate = fs::read(root.join(SITE_TRUST_DIRECTORY).join(NODE_CERTIFICATE_FILE));
    match certificate.ok().as_deref().and_then(certificate_time) {
        Some(CertificateTime::Expired) => NodeFolderError::CertificateExpired {
            root: root.to_owned(),
        },
        Some(CertificateTime::NotYetValid) => NodeFolderError::CertificateNotYetValid {
            root: root.to_owned(),
        },
        Some(CertificateTime::Valid) | None => NodeFolderError::Invalid(error),
    }
}

fn staging_path(root: &Path) -> Result<PathBuf, NodeFolderError> {
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| NodeFolderError::NoParent {
            root: root.to_owned(),
        })?;
    Ok(root.with_file_name(format!(
        ".{name}.join-{}-{}",
        std::process::id(),
        STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )))
}

fn create_owner_directory(path: &Path) -> Result<(), NodeFolderError> {
    atomic::create_owner_directory(path)
        .map_err(|source| storage("create owner-only directory", path, source))
}

fn write_owner_file(path: &Path, bytes: &[u8]) -> Result<(), NodeFolderError> {
    atomic::write_new_owner_file(path, bytes)
        .map_err(|source| storage("write owner-only file", path, source))
}

fn sync_directory(path: &Path) -> Result<(), NodeFolderError> {
    dotcore_domain::fs_sync::sync_directory(path)
        .map_err(|source| storage("sync directory", path, source))
}

fn c_paths(from: &Path, to: &Path) -> io::Result<(std::ffi::CString, std::ffi::CString)> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let from = CString::new(from.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source path contains NUL"))?;
    let to = CString::new(to.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target path contains NUL"))?;
    Ok((from, to))
}

fn storage(operation: &str, path: &Path, source: io::Error) -> NodeFolderError {
    NodeFolderError::Storage(
        anyhow::Error::new(source).context(format!("{operation} {}", path.display())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_overwrite_rename_refuses_an_existing_target() {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("directory");
        let from = directory.path().join("from");
        let to = directory.path().join("to");
        fs::create_dir(&from).expect("source");
        fs::create_dir(&to).expect("target");
        fs::write(to.join("kept"), b"existing").expect("existing content");

        let error = rename_noreplace(&from, &to).expect_err("existing target");

        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert!(from.is_dir());
        assert_eq!(fs::read(to.join("kept")).expect("kept"), b"existing");

        fs::remove_file(to.join("kept")).expect("clear target");
        fs::remove_dir(&to).expect("remove target");
        rename_noreplace(&from, &to).expect("rename to a free target");
        assert!(to.is_dir());
        assert!(!from.exists());
    }
}
