//! The site's own manager node re-scanning its targets: this machine holds the site, so the
//! command reads the stopped site and this folder's node binding, scans the machine for the
//! capabilities the manager node's committed targets offer, and records the whole replacement
//! through the same live-site membership write the running manager uses.
//!
//! There is no handshake and no second approval: the operator reviews and confirms the change in
//! the terminal of this machine. The site is held exclusively from planning through the write, so a
//! running site manager, or another site command writing the site, is refused before anything is
//! read. The replacement goes through the same derivation a worker's proposal goes through — the
//! whole-node base, the deployment guard, the whole-site check and the whole-projection no-op — and
//! the write re-checks that the node still holds the base that was confirmed. A manager node has no
//! certificate pin of its own: its identity is the site's trust material, which a re-scan leaves
//! untouched. Nothing is written when the whole projection already equals the replacement, a failed
//! write leaves the site holding the declaration it had, and the command never restarts the site: it
//! reports the restart the replacement needs.

use std::path::{Path, PathBuf};

use dotcore_proto::dotcore::{
    node::v1::{DeclaredMachine, NodeConfiguration},
    site::v1::{ExecutionTarget, Site, SiteNode},
};
use dotcore_proto::yaml;

use super::{
    RescanBase, RescanCommitError, RescanError, RescanReplacement, RescanScan, RescanScanError,
    commit::{ReplacementRole, record_replacement},
    derive_replacement, rescan_base,
    scan::{scan_replacement, scan_request},
};
use crate::{
    NODE_FILE, SITE_FILE,
    initial::{
        input_store::{SiteInputLock, SiteInputLockError, try_acquire_exclusive_site_lock},
        probe::InitialSiteProbeReport,
        request::InitialSiteRequest,
        validation::{SiteInputBundleValidator, SiteInputValidationError},
    },
    node::binding::{NodeBindingError, bound_site_node},
    repository::{SiteRepository, SiteRepositoryError},
    validation::{SiteDeclarationRules, load_validated_site},
};

#[cfg(test)]
use crate::repository::SiteWriteFaults;

/// The site folder of the machine that manages the site, opened for a re-scan of its own manager
/// node. The site is held exclusively while the replacement is planned and recorded, so this
/// command is the only writer of `site.yaml` between the operator's confirmation and the write.
pub struct ManagerNodeRescan {
    site_root: PathBuf,
    site_id: String,
    repository: SiteRepository,
    node: SiteNode,
    declared_machine: Option<DeclaredMachine>,
    committed: Vec<ExecutionTarget>,
    _site_lock: SiteInputLock,
}

impl ManagerNodeRescan {
    /// Opens the site folder at `site_root` to re-scan the manager node it binds: it must hold a
    /// site, its site must not be running or locked by another writer, and its `NODE_FILE` binding
    /// must name this site's manager node.
    pub fn open(
        site_root: &Path,
        validator: &SiteInputBundleValidator,
    ) -> Result<Self, ManagerNodeRescanError> {
        let lock = Self::hold(site_root, validator)?;
        let repository =
            SiteRepository::load(&site_root.join(SITE_FILE), validator.declaration_rules())
                .map_err(site_error(site_root))?;
        Self::bind(site_root, repository, lock)
    }

    /// Refuses a folder that holds no site, takes the site lock a running site manager or another
    /// site command would hold, and validates the site folder's bundle under that lock.
    fn hold(
        site_root: &Path,
        validator: &SiteInputBundleValidator,
    ) -> Result<SiteInputLock, ManagerNodeRescanError> {
        if !site_root.join(SITE_FILE).is_file() {
            return Err(ManagerNodeRescanError::NotSiteFolder {
                site_root: site_root.to_owned(),
            });
        }
        let lock = try_acquire_exclusive_site_lock(site_root)
            .map_err(|source| ManagerNodeRescanError::SiteLock {
                site_root: site_root.to_owned(),
                source,
            })?
            .ok_or_else(|| ManagerNodeRescanError::SiteRunning {
                site_root: site_root.to_owned(),
            })?;
        validator
            .validate_without_runtime_declarations(site_root)
            .map_err(|source| ManagerNodeRescanError::InvalidSiteFolder {
                site_root: site_root.to_owned(),
                source,
            })?;
        Ok(lock)
    }

    /// Returns the identity of the site whose manager node is re-scanned.
    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    /// Returns the identity of the re-scanned manager node.
    pub fn node_id(&self) -> &str {
        &self.node.node_id
    }

    /// Returns the site folder the re-scan reads and writes.
    pub fn site_root(&self) -> &Path {
        &self.site_root
    }

    /// Returns the execution targets the manager node holds now: the base a replacement replaces.
    pub fn committed_targets(&self) -> &[ExecutionTarget] {
        &self.committed
    }

    /// Returns the request that probes this machine for every capability the manager node can
    /// offer, which the caller probes before rebuilding its targets.
    pub fn scan_request(&self) -> InitialSiteRequest {
        scan_request(&self.committed, self.declared_machine.as_ref())
    }

    /// Rebuilds the manager node's whole target set from a fresh scan of this machine.
    ///
    /// The scan offers every capability this machine has, so a capability gained since the node was
    /// declared adds its targets; a committed capability the machine no longer offers keeps its
    /// committed targets and is reported by the scan.
    pub fn scan(
        &self,
        probe: &InitialSiteProbeReport,
    ) -> Result<RescanScan, ManagerNodeRescanError> {
        scan_replacement(
            self.node_id(),
            &self.committed,
            self.declared_machine.as_ref(),
            probe,
            std::env::consts::ARCH,
        )
        .map_err(ManagerNodeRescanError::Scan)
    }

    /// Derives and validates the whole replacement `targets` would leave, without writing: the
    /// deployment guard and the whole-site check refuse a replacement before the operator is asked
    /// to confirm it.
    pub fn plan(
        &self,
        targets: Vec<ExecutionTarget>,
    ) -> Result<RescanReplacement, ManagerNodeRescanError> {
        let site = self.durable_site()?;
        let base = self.approved_base(&site)?;
        derive_replacement(&site, &self.rules(), self.node_id(), targets, Some(&base))
            .map_err(ManagerNodeRescanError::Refused)
    }

    /// Records the confirmed `plan` in the site: the durable declaration is read again and the
    /// replacement derived from it once more, so the node must still hold the whole-node base that
    /// was confirmed, its deployments must still guard every changed and removed target, and the
    /// whole site must still be valid with the replacement. The replacement that passes is what is
    /// written; a no-op writes nothing and needs no restart, and a write whose outcome stays
    /// uncertain is decided by the durable declaration, never by guessing.
    pub fn commit(
        &self,
        plan: &RescanReplacement,
    ) -> Result<ManagerRescanOutcome, ManagerNodeRescanError> {
        let site = self.durable_site()?;
        let base = self.approved_base(&site)?;
        let replacement = derive_replacement(
            &site,
            &self.rules(),
            self.node_id(),
            plan.targets().to_vec(),
            Some(&base),
        )
        .map_err(ManagerNodeRescanError::Refused)?;
        if replacement.is_noop() {
            return Ok(ManagerRescanOutcome::Unchanged);
        }

        match record_replacement(
            &self.repository,
            &base,
            &replacement,
            ReplacementRole::SiteManager,
        ) {
            Ok(()) => Ok(ManagerRescanOutcome::Recorded),
            Err(error @ RescanCommitError::OutcomeUncertain(_)) => {
                // The write may have landed. The durable declaration decides, and a recorded
                // replacement is never rolled back.
                if self.durable_declaration_matches(&replacement)? {
                    Ok(ManagerRescanOutcome::Recorded)
                } else {
                    Err(ManagerNodeRescanError::Commit(error))
                }
            }
            Err(error) => Err(ManagerNodeRescanError::Commit(error)),
        }
    }

    /// Returns whether `site.yaml` declares the replacement, which decides a write that ended
    /// without a known outcome.
    fn durable_declaration_matches(
        &self,
        replacement: &RescanReplacement,
    ) -> Result<bool, ManagerNodeRescanError> {
        let site = load_validated_site(&self.site_root.join(SITE_FILE), &self.rules())
            .map_err(site_error(&self.site_root))?;
        let base = rescan_base(&site, self.node_id()).map_err(ManagerNodeRescanError::Refused)?;
        Ok(base.targets == replacement.targets() && base.observation == *replacement.observation())
    }

    /// Returns the whole-node base of the manager node in the durable `site`, which every
    /// replacement is approved against.
    fn approved_base(&self, site: &Site) -> Result<RescanBase, ManagerNodeRescanError> {
        rescan_base(site, self.node_id()).map_err(ManagerNodeRescanError::Refused)
    }

    /// Returns the durable site the replacement is checked against.
    fn durable_site(&self) -> Result<Site, ManagerNodeRescanError> {
        self.repository
            .snapshot()
            .map_err(site_error(&self.site_root))
    }

    /// Returns the owner rules the site's declarations must satisfy.
    fn rules(&self) -> SiteDeclarationRules {
        self.repository.declaration_rules()
    }

    /// Binds the opened site to the manager node this folder declares, reading the committed targets
    /// and the machine the node declares in place of its hardware.
    fn bind(
        site_root: &Path,
        repository: SiteRepository,
        lock: SiteInputLock,
    ) -> Result<Self, ManagerNodeRescanError> {
        let site = repository.snapshot().map_err(site_error(site_root))?;
        let node: NodeConfiguration =
            yaml::from_file(&site_root.join(NODE_FILE)).map_err(|source| {
                ManagerNodeRescanError::NodeConfiguration {
                    site_root: site_root.to_owned(),
                    source,
                }
            })?;
        let bound = bound_site_node(&site, &node).map_err(|source| {
            ManagerNodeRescanError::NodeBinding {
                site_root: site_root.to_owned(),
                source,
            }
        })?;
        if bound.declaration.site_manager.is_none() {
            return Err(ManagerNodeRescanError::NotManagerNode {
                node_id: bound.node_id.as_str().to_owned(),
            });
        }
        let committed = site
            .execution_targets
            .iter()
            .filter(|target| target.site_node_id == bound.node_id.as_str())
            .cloned()
            .collect();
        Ok(Self {
            site_root: site_root.to_owned(),
            site_id: site.site_id,
            repository,
            node: bound.declaration,
            declared_machine: node.declared_machine,
            committed,
            _site_lock: lock,
        })
    }

    /// The same opening with a repository whose `site.yaml` writes follow queued faults, for tests
    /// of what a write that fails or ends without a known outcome leaves behind. The folder is
    /// still held exclusively and validated, so only the write seam differs.
    #[cfg(test)]
    pub(super) fn open_with_write_faults(
        site_root: &Path,
        validator: &SiteInputBundleValidator,
    ) -> Result<(Self, SiteWriteFaults), ManagerNodeRescanError> {
        let lock = Self::hold(site_root, validator)?;
        let (repository, faults) =
            SiteRepository::load_with_write_faults_for_test(&site_root.join(SITE_FILE))
                .map_err(site_error(site_root))?;
        Ok((Self::bind(site_root, repository, lock)?, faults))
    }
}

/// What a manager node's re-scan left the site with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManagerRescanOutcome {
    /// The site recorded the replacement durably. It serves under it only after it restarts.
    Recorded,

    /// The whole projection equals what the site already declares: nothing was written and no
    /// restart is needed.
    Unchanged,
}

/// Why a site's manager node could not re-scan its targets.
#[derive(Debug, thiserror::Error)]
pub enum ManagerNodeRescanError {
    /// The folder holds no site, so it is not the folder of the machine that manages one.
    #[error(
        "{site_root} holds no site (no site.yaml); run the re-scan in a site folder, or in a joined node folder"
    )]
    NotSiteFolder {
        /// Folder that was opened.
        site_root: PathBuf,
    },

    /// A running site manager, or another site command writing the site, holds the site.
    #[error(
        "the site at {site_root} is running or locked; stop this machine's site before re-scanning its manager node's targets"
    )]
    SiteRunning {
        /// Site folder whose lock is held.
        site_root: PathBuf,
    },

    /// The site lock could not be taken for a reason other than another holder.
    #[error("lock the site at {site_root}")]
    SiteLock {
        /// Site folder whose lock could not be taken.
        site_root: PathBuf,

        /// Underlying lock failure.
        #[source]
        source: SiteInputLockError,
    },

    /// The site folder could not be read or violates its contract.
    #[error("read the site folder {site_root}")]
    InvalidSiteFolder {
        /// Folder that was opened.
        site_root: PathBuf,

        /// Underlying folder failure.
        #[source]
        source: SiteInputValidationError,
    },

    /// The site could not be read or written.
    #[error("read the site at {site_root}")]
    Site {
        /// Site folder whose site failed to open.
        site_root: PathBuf,

        /// Underlying site failure.
        #[source]
        source: SiteRepositoryError,
    },

    /// The folder's node configuration could not be read.
    #[error("read the node configuration of the site at {site_root}")]
    NodeConfiguration {
        /// Folder whose node configuration was read.
        site_root: PathBuf,

        /// Underlying configuration failure.
        #[source]
        source: dotcore_proto::yaml::RestrictedYamlError,
    },

    /// The folder's node binding names no declared node of this site.
    #[error("{} does not bind a declared node of its site", site_root.display())]
    NodeBinding {
        /// Folder whose binding was read.
        site_root: PathBuf,

        /// Underlying binding failure.
        #[source]
        source: NodeBindingError,
    },

    /// The bound node is not this site's manager node: a joined node proposes its replacement to
    /// the site manager instead.
    #[error(
        "{node_id:?} is not this site's manager node; a joined node re-scans by proposing its replacement to the site manager"
    )]
    NotManagerNode {
        /// Node the folder binds.
        node_id: String,
    },

    /// The replacement target set could not be built from the scan.
    #[error(transparent)]
    Scan(#[from] RescanScanError),

    /// The replacement was refused: its node's base moved, a changed or removed target carries
    /// deployments, or the site with the replacement would be invalid. Nothing was written.
    #[error(transparent)]
    Refused(#[from] RescanError),

    /// The site did not record the replacement, or the write ended without a known outcome.
    #[error(transparent)]
    Commit(#[from] RescanCommitError),
}

/// Names a site read or write failure by the folder it happened in.
fn site_error(site_root: &Path) -> impl FnOnce(SiteRepositoryError) -> ManagerNodeRescanError + '_ {
    move |source| ManagerNodeRescanError::Site {
        site_root: site_root.to_owned(),
        source,
    }
}
