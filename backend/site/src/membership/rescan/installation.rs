//! Installs the projection a re-scan committed into the node's own folder: the whole target set,
//! the refreshed observation configuration and the digest the site recorded for it.
//!
//! The node never writes the site; it writes only its own manifest, and it does so in one rename
//! that replaces the previous manifest, so the node either installs the committed projection or
//! keeps the one it had. A projection already installed is left untouched, which is what makes a
//! no-op write no bytes at all. The manifest is read back and validated after the rename, so the
//! node never starts under a projection it would reject at startup.

use std::path::Path;

use dotcore_proto::dotcore::{node::v1::NodeObservationConfiguration, site::v1::ExecutionTarget};

use crate::{
    atomic::StdFileSystem,
    membership::join::{NodeFolderError, rewrite_bootstrap},
    node::{
        folder::{
            NodeFolderBootstrap, NodeFolderContractError, NodeFolderValidationError, read_bootstrap,
        },
        observation_configuration::{
            NodeObservationConfigurationDigest, NodeObservationConfigurationError,
        },
    },
};

/// The whole projection one node installs, as the site recorded it.
#[derive(Clone, Debug, PartialEq)]
pub struct InstalledProjection {
    /// Whole target set the node installs.
    pub targets: Vec<ExecutionTarget>,

    /// Refreshed observation configuration the node installs.
    pub observation: NodeObservationConfiguration,

    /// Canonical lowercase SHA-256 digest of the committed observation configuration.
    pub observation_configuration_sha256: String,
}

/// Installs `projection` in the node folder at `root`, and reports whether any byte changed.
///
/// The committed digest is recomputed from the committed observation configuration before anything
/// is written: a projection that does not carry its own digest is refused without a write. The
/// manifest the folder holds is re-read here, so the comparison is against what the node installs
/// now, and a folder that already installs exactly this projection writes nothing. The replacement
/// manifest is composed and validated before the rename, and read back and validated after it, so
/// the node never starts under a projection it would reject at startup.
pub fn install_projection(
    root: &Path,
    projection: &InstalledProjection,
) -> Result<bool, RescanInstallError> {
    let digest = NodeObservationConfigurationDigest::for_configuration(&projection.observation)
        .map_err(RescanInstallError::Derivation)?;
    if digest.as_str() != projection.observation_configuration_sha256 {
        return Err(RescanInstallError::DigestMismatch {
            committed: projection.observation_configuration_sha256.clone(),
            computed: digest.as_str().to_owned(),
        });
    }

    let installed = read_bootstrap(root).map_err(|source| RescanInstallError::Unreadable {
        node_root: root.to_owned(),
        source,
    })?;
    let replacement = projection_folder_bootstrap(&installed, projection)?;
    if replacement == installed {
        return Ok(false);
    }
    rewrite_bootstrap(&StdFileSystem, root, &replacement)
        .map_err(|source| RescanInstallError::write(root, source))?;
    // The manifest is read back and validated, so the node never starts under a projection it
    // would reject at startup.
    let written = read_bootstrap(root).map_err(|source| RescanInstallError::NotInstalled {
        node_root: root.to_owned(),
        source,
    })?;
    if written != replacement {
        return Err(RescanInstallError::Changed {
            node_root: root.to_owned(),
        });
    }
    Ok(true)
}

/// Composes the manifest the node holds after installing `projection`, keeping what a re-scan does
/// not change: its identities, its pinned manager, its private bind address and its declared
/// machine.
fn projection_folder_bootstrap(
    installed: &NodeFolderBootstrap,
    projection: &InstalledProjection,
) -> Result<NodeFolderBootstrap, RescanInstallError> {
    NodeFolderBootstrap::new(
        &installed.site_id,
        &installed.worker_node_id,
        installed.worker_certificate_sha256.clone(),
        installed.manager.clone(),
        projection.observation.clone(),
        projection.targets.clone(),
        installed.private_bind_address.clone(),
    )
    .and_then(|bootstrap| bootstrap.with_declared_machine(installed.declared_machine.clone()))
    .map_err(RescanInstallError::Manifest)
}

/// Why a committed projection could not be installed in the node folder.
#[derive(Debug, thiserror::Error)]
pub enum RescanInstallError {
    /// The committed observation configuration does not carry the digest it digests to, so the
    /// projection is not the one the site described.
    #[error(
        "the committed observation configuration digests to {computed}, not the committed {committed}; nothing was installed"
    )]
    DigestMismatch {
        /// Digest the site recorded.
        committed: String,

        /// Digest the committed observation configuration digests to.
        computed: String,
    },

    /// The committed projection could not be composed into a node manifest.
    #[error("compose the node manifest with the committed projection: {0}")]
    Manifest(#[source] NodeFolderContractError),

    /// The refreshed observation configuration could not be digested.
    #[error("digest the committed observation configuration: {0}")]
    Derivation(#[source] NodeObservationConfigurationError),

    /// The manifest write had no known outcome; the node must not start until the folder is read
    /// again and the re-scan is run once more.
    #[error(
        "installing the committed projection in {node_root} ended without a known outcome; the node must not start until the re-scan is run again"
    )]
    WriteUncertain {
        /// Node folder whose manifest write is uncertain.
        node_root: std::path::PathBuf,

        /// Underlying write failure.
        #[source]
        source: NodeFolderError,
    },

    /// The manifest was not replaced; the folder keeps the projection it had.
    #[error("install the committed projection in {node_root}")]
    Write {
        /// Node folder whose manifest could not be replaced.
        node_root: std::path::PathBuf,

        /// Underlying write failure.
        #[source]
        source: NodeFolderError,
    },

    /// The node folder holds no manifest that can be read and validated.
    #[error("read the node manifest of {node_root}")]
    Unreadable {
        /// Node folder whose manifest was read.
        node_root: std::path::PathBuf,

        /// Why the folder does not read as a joined node.
        #[source]
        source: NodeFolderValidationError,
    },

    /// The manifest could not be read back and validated after the rename.
    #[error("read the node manifest in {node_root} after installing the committed projection")]
    NotInstalled {
        /// Node folder whose manifest was read back.
        node_root: std::path::PathBuf,

        /// Why the read-back folder does not validate.
        #[source]
        source: NodeFolderValidationError,
    },

    /// The manifest was replaced, but reads back as a different projection than the one installed.
    #[error(
        "the node manifest in {node_root} reads back as another projection than the one installed"
    )]
    Changed {
        /// Node folder whose manifest was read back.
        node_root: std::path::PathBuf,
    },
}

impl RescanInstallError {
    fn write(node_root: &std::path::Path, source: NodeFolderError) -> Self {
        if matches!(source, NodeFolderError::RewriteUncertain(_)) {
            return Self::WriteUncertain {
                node_root: node_root.to_owned(),
                source,
            };
        }
        Self::Write {
            node_root: node_root.to_owned(),
            source,
        }
    }
}
