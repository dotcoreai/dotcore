//! The node's side of a target re-scan: it proposes its whole replacement target set to the site
//! manager over the pinned identity it already holds, then installs the projection the manager
//! records.
//!
//! The folder is re-read and re-validated under the node custody lock, so a running node never has
//! its targets changed under it. The proposal carries the node's own certificate and the observation
//! configuration the folder installs today, which is how the manager tells a whole projection that
//! needs no bytes at all from one the node must install. The node never writes the site: it installs
//! only what the manager committed, and reports the restart that serving under it needs.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use dotcore_proto::dotcore::{
    node::v1::{
        ProposeRescanRequest, node_rescan_service_client::NodeRescanServiceClient,
        propose_rescan_response::Outcome,
    },
    site::v1::ExecutionTarget,
};
use tonic::{Code, Status};
use url::Url;

use super::{
    RescanScan, RescanScanError,
    installation::{InstalledProjection, install_projection},
    scan::{scan_replacement, scan_request},
};

use crate::{
    SITE_FILE,
    node::{
        agent::WorkerPrivateClientConfiguration,
        folder::{
            NodeFolderValidator, ValidatedNodeFolder, WORKER_MANIFEST_FILE, WorkerNodeCustody,
            WorkerNodeCustodyError,
        },
    },
};

/// A joined node's folder opened for a target re-scan, with custody of the folder held so the node's
/// server is not running while its targets change.
pub struct NodeRescan {
    node_root: PathBuf,
    folder: ValidatedNodeFolder,
    _custody: WorkerNodeCustody,
}

impl NodeRescan {
    /// Re-reads and re-validates the node folder at `node_root` and takes its custody lock. Refuses
    /// a manager site folder, a folder that is not a joined node folder, and a node whose server is
    /// running.
    pub fn open(node_root: &Path) -> Result<Self, NodeRescanError> {
        require_joined_node_folder(node_root)?;
        let custody = WorkerNodeCustody::try_acquire(node_root).map_err(|source| match source {
            WorkerNodeCustodyError::AlreadyHeld => NodeRescanError::NodeRunning {
                node_root: node_root.to_owned(),
            },
            source => NodeRescanError::Custody {
                node_root: node_root.to_owned(),
                source,
            },
        })?;
        let folder = NodeFolderValidator::validate_declared(node_root).map_err(|source| {
            NodeRescanError::InvalidNodeFolder {
                node_root: node_root.to_owned(),
                source,
            }
        })?;
        Ok(Self {
            node_root: node_root.to_owned(),
            folder,
            _custody: custody,
        })
    }

    /// Returns the identity of the site this node belongs to.
    pub fn site_id(&self) -> &str {
        self.folder.site_id()
    }

    /// Returns the identity of the rescanned SiteNode.
    pub fn node_id(&self) -> &str {
        self.folder.worker_node_id()
    }

    /// Returns the node folder under custody.
    pub fn node_root(&self) -> &Path {
        &self.node_root
    }

    /// Returns the request that probes this machine for every capability this node can offer,
    /// which the caller probes before asking for the replacement.
    pub fn scan_request(&self) -> crate::initial::request::InitialSiteRequest {
        scan_request(
            self.folder.realization_targets(),
            self.folder.stored_declared_machine(),
        )
    }

    /// Rebuilds this node's whole target set from a fresh scan of this machine.
    ///
    /// The scan offers every capability this machine has, so a capability gained since the node was
    /// declared adds its targets; a committed capability the machine no longer offers keeps its
    /// committed targets and is reported by the scan.
    pub fn scan(
        &self,
        probe: &crate::initial::probe::InitialSiteProbeReport,
    ) -> Result<RescanScan, NodeRescanError> {
        scan_replacement(
            self.folder.worker_node_id(),
            self.folder.realization_targets(),
            self.folder.stored_declared_machine(),
            probe,
            std::env::consts::ARCH,
        )
        .map_err(NodeRescanError::Scan)
    }

    /// Returns the execution targets the node installs now: the base a replacement replaces.
    pub fn committed_targets(&self) -> &[ExecutionTarget] {
        self.folder.realization_targets()
    }

    /// Proposes `targets` as this node's whole replacement target set and installs what the site
    /// manager commits.
    ///
    /// Nothing is written when the manager answers that the whole projection is unchanged, or when
    /// the folder already installs the committed projection. An answer that never arrives, and a
    /// manifest write whose outcome is unknown, both leave the folder holding the projection it had.
    pub async fn propose(
        &self,
        targets: Vec<ExecutionTarget>,
    ) -> Result<NodeRescanOutcome, NodeRescanError> {
        let projection = self.answer(targets).await?;
        let Some(projection) = projection else {
            return Ok(NodeRescanOutcome::Unchanged);
        };
        let installed =
            install_projection(&self.node_root, &projection).map_err(NodeRescanError::Install)?;
        Ok(NodeRescanOutcome::Installed {
            restart_required: installed,
        })
    }

    /// Connects over the node's own pinned identity and returns the projection the site manager
    /// committed, or `None` when it answered that the whole projection is unchanged.
    async fn answer(
        &self,
        targets: Vec<ExecutionTarget>,
    ) -> Result<Option<InstalledProjection>, NodeRescanError> {
        let manager_endpoint = self.folder.manager_endpoint().clone();
        let configuration = WorkerPrivateClientConfiguration::from_node_folder(&self.folder);
        let channel = configuration
            .handshake_endpoint()
            .map_err(|source| NodeRescanError::Unreachable {
                endpoint: manager_endpoint.clone(),
                source,
            })?
            .connect()
            .await
            .map_err(|source| NodeRescanError::Unreachable {
                endpoint: manager_endpoint,
                source: anyhow::Error::new(source),
            })?;
        let request = ProposeRescanRequest {
            node_certificate_der: self.folder.worker_certificate_der().to_vec(),
            node_id: self.folder.worker_node_id().to_owned(),
            targets,
            observation: Some(self.folder.observation_configuration().clone()),
        };
        let answer = NodeRescanServiceClient::new(channel)
            .propose_rescan(request)
            .await
            .map_err(|status| self.refusal(status))?
            .into_inner();
        match answer.outcome {
            Some(Outcome::Committed(committed)) => Ok(Some(InstalledProjection {
                targets: committed.targets,
                observation: committed
                    .observation
                    .ok_or(NodeRescanError::IncompleteAnswer {
                        reason: "the committed projection carries no observation configuration"
                            .to_owned(),
                    })?,
                observation_configuration_sha256: committed.observation_configuration_sha256,
            })),
            Some(Outcome::NoOp(_)) => Ok(None),
            None => Err(NodeRescanError::IncompleteAnswer {
                reason: "the site manager answered without an outcome".to_owned(),
            }),
        }
    }

    /// Projects the manager's refusal. A code that leaves the commit's outcome unknown, including a
    /// connection that ended mid-request, is reported as such: the node installs nothing and the
    /// re-scan is run again, where the durable declaration decides.
    fn refusal(&self, status: Status) -> NodeRescanError {
        let reason = status.message().to_owned();
        match status.code() {
            Code::NotFound => NodeRescanError::NotDeclared {
                node_id: self.folder.worker_node_id().to_owned(),
                reason,
            },
            Code::Internal
            | Code::Unknown
            | Code::Unavailable
            | Code::Cancelled
            | Code::DataLoss => NodeRescanError::OutcomeUnknown { reason },
            _ => NodeRescanError::Refused { reason },
        }
    }
}

/// What a re-scan left the node with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRescanOutcome {
    /// The site recorded the replacement. The node installed it, and `restart_required` says
    /// whether serving under it needs a restart.
    Installed { restart_required: bool },

    /// The whole projection equals what the node already installs: nothing was written anywhere and
    /// no restart is needed.
    Unchanged,
}

/// Why a node could not re-scan its targets.
#[derive(Debug, thiserror::Error)]
pub enum NodeRescanError {
    /// The folder holds a manager site configuration.
    #[error(
        "{node_root} holds a site (site.yaml), not a joined node; re-scan the manager's own targets with the command on this machine"
    )]
    ManagerFolder {
        /// Folder that was opened.
        node_root: PathBuf,
    },

    /// The folder is empty, absent, or holds something other than a joined node.
    #[error("{node_root} is not a joined node folder (no worker.yaml)")]
    NotJoinedNodeFolder {
        /// Folder that was opened.
        node_root: PathBuf,
    },

    /// Another process holds the node custody lock.
    #[error("the node server for {node_root} is running; stop it before re-scanning its targets")]
    NodeRunning {
        /// Folder whose custody lock is held.
        node_root: PathBuf,
    },

    /// The custody lock could not be taken for a reason other than another holder.
    #[error("take custody of the node folder {node_root}: {source}")]
    Custody {
        /// Folder that was opened.
        node_root: PathBuf,

        /// Underlying custody failure.
        #[source]
        source: WorkerNodeCustodyError,
    },

    /// The node folder could not be read or violates its contract.
    #[error("read the node folder {node_root}")]
    InvalidNodeFolder {
        /// Folder that was opened.
        node_root: PathBuf,

        /// Underlying folder failure.
        #[source]
        source: crate::node::folder::NodeFolderValidationError,
    },

    /// The node's folder could not be inspected.
    #[error("inspect {path}: {source}")]
    Inspection {
        /// Path that could not be inspected.
        path: PathBuf,

        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// The site manager could not be reached, or the channel could not be built.
    #[error("reach the site manager at {endpoint}: {source}")]
    Unreachable {
        /// Manager endpoint the node dialed.
        endpoint: Url,

        /// Underlying transport failure.
        #[source]
        source: anyhow::Error,
    },

    /// The site manager says this node is not declared in the site.
    #[error("this node is not declared in its site: {reason}")]
    NotDeclared {
        /// Node the proposal named.
        node_id: String,

        /// Reason the manager gave.
        reason: String,
    },

    /// The site manager refused the replacement; the node and the site are unchanged.
    #[error("the site manager refused the replacement: {reason}")]
    Refused {
        /// Reason the manager gave.
        reason: String,
    },

    /// The re-scan's outcome is unknown: the site may or may not have recorded the replacement. The
    /// node installed nothing, and the re-scan is run again to compare against durable state.
    #[error(
        "the site manager could not confirm the outcome: {reason}; nothing was installed, so run the re-scan again once the cause is resolved"
    )]
    OutcomeUnknown {
        /// Reason the manager gave, or why the answer never arrived.
        reason: String,
    },

    /// The manager's answer did not carry a whole projection.
    #[error("the site manager's answer was incomplete: {reason}")]
    IncompleteAnswer {
        /// What the answer lacked.
        reason: String,
    },

    /// The committed projection could not be installed in the node folder.
    #[error(transparent)]
    Install(#[from] super::installation::RescanInstallError),

    /// The replacement target set could not be built from the scan.
    #[error(transparent)]
    Scan(#[from] RescanScanError),
}

/// Refuses a folder that is not a joined node folder before any lock is created.
fn require_joined_node_folder(node_root: &Path) -> Result<(), NodeRescanError> {
    if exists(&node_root.join(SITE_FILE))? {
        return Err(NodeRescanError::ManagerFolder {
            node_root: node_root.to_owned(),
        });
    }
    let manifest = node_root.join(WORKER_MANIFEST_FILE);
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(NodeRescanError::NotJoinedNodeFolder {
            node_root: node_root.to_owned(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(NodeRescanError::NotJoinedNodeFolder {
                node_root: node_root.to_owned(),
            })
        }
        Err(source) => Err(NodeRescanError::Inspection {
            path: manifest,
            source,
        }),
    }
}

fn exists(path: &Path) -> Result<bool, NodeRescanError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(NodeRescanError::Inspection {
            path: path.to_owned(),
            source,
        }),
    }
}
