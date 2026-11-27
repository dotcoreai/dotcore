//! The durable half of a re-scan: the site writer that replaces one declared node's whole target
//! set, and the error each outcome maps to.
//!
//! The write is persist-or-reject like every other membership change: the replacement reaches the
//! running site only once `site.yaml` holds it, and a replacement equal to what the site already
//! declares writes nothing. The caller re-checks the pin, the whole-node base and the exact
//! replacement before calling; the writer checks again under the site lock that the node is still
//! the declaration the approval was for — a joined worker or this site's manager node — that it
//! still holds the base it was approved against, and that no deployment has since been placed on a
//! target the replacement changes or removes, and refuses the whole replacement otherwise. Nothing
//! rolls a recorded replacement back.

use dotcore_proto::dotcore::site::v1::Site;

use super::{RescanBase, RescanDeploymentImpact, RescanReplacement, deployment_guard_lines};
use crate::{
    node::folder::CertificateFingerprint,
    persistence::SiteStorageError,
    repository::{
        MembershipSiteTransaction, ScopedSiteMutationError, SiteRepository, SiteRepositoryError,
    },
    validation::SiteDeclarationRules,
};

/// Reads the running site and replaces one node's whole target set durably.
pub trait RescanSiteWriter: Send + Sync + 'static {
    /// Returns the owner rules every site this writer reads or writes must satisfy.
    fn declaration_rules(&self) -> SiteDeclarationRules;

    /// Returns the current site the replacement is checked against.
    fn site(&self) -> Result<Site, RescanCommitError>;

    /// Returns the certificate pin the site trust holds for `node_id`, or `None` without one.
    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, RescanCommitError>;

    /// Replaces the whole execution-target set of the node `replacement` was approved for with the
    /// replacement's targets, completely or not at all. Nothing is written when the site already
    /// declares exactly the replacement's targets.
    fn commit_replacement(
        &self,
        base: &RescanBase,
        replacement: &RescanReplacement,
    ) -> Result<(), RescanCommitError>;
}

/// Why a re-scan's replacement was not recorded.
#[derive(Debug, thiserror::Error)]
pub enum RescanCommitError {
    /// The site refuses the replacement: its node is gone, its base moved, or the site with the
    /// replacement would be invalid.
    #[error("the site refuses this replacement: {0}")]
    Rejected(#[source] anyhow::Error),

    /// The replacement could not be recorded, and the site holds what it held before.
    #[error("the site could not record the replacement; nothing was written: {0}")]
    Failed(#[source] anyhow::Error),

    /// The write ended without a known outcome; `site.yaml` may hold the replacement.
    #[error("the site may or may not have recorded the replacement: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),
}

/// Why the whole-target replacement was refused under the site lock.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ReplacementChangeError {
    /// The node is absent or is no longer a declared worker.
    #[error("node {node_id:?} is no longer a declared worker of this site")]
    NodeNotDeclared { node_id: String },

    /// The node is absent or is no longer this site's manager node.
    #[error("node {node_id:?} is no longer this site's manager node")]
    ManagerNotDeclared { node_id: String },

    /// The node's committed targets no longer match the whole-node base the replacement was
    /// approved against, so the replacement must be proposed again.
    #[error(
        "the node's base no longer matches the durable declaration; propose the replacement again"
    )]
    StaleBase,

    /// A deployment, including one pending teardown, is placed on a target the replacement
    /// changes or removes.
    #[error(
        "the replacement would remove or change targets with deployments placed on them:{}\nthe whole replacement is refused",
        deployment_guard_lines(.impacts)
    )]
    DeploymentGuard {
        impacts: Vec<RescanDeploymentImpact>,
    },
}

/// Which declaration a replacement is approved for: a joined worker, or the site's own manager
/// node. The role decides what the site lock re-checks about the node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplacementRole {
    Worker,
    SiteManager,
}

/// Replaces one node's execution targets in `site` with `replacement`'s targets.
///
/// Under the site lock the node must still hold the declaration `role` names, its committed
/// targets must still be exactly `base`'s, which is the whole-node base check the approval is bound
/// to, and no deployment may be placed on a target the replacement changes or removes. The
/// replacement's own targets were validated against the whole site before it was offered, and the
/// repository validates the resulting site again before writing it.
pub(crate) fn replace_targets(
    site: &mut MembershipSiteTransaction<'_>,
    base: &RescanBase,
    replacement: &RescanReplacement,
    role: ReplacementRole,
) -> Result<(), ReplacementChangeError> {
    let node_id = base.node.node_id.as_str();
    let declared = site
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .filter(|node| match role {
            ReplacementRole::Worker => node.site_manager.is_none(),
            ReplacementRole::SiteManager => node.site_manager.is_some(),
        })
        .ok_or_else(|| match role {
            ReplacementRole::Worker => ReplacementChangeError::NodeNotDeclared {
                node_id: node_id.to_owned(),
            },
            ReplacementRole::SiteManager => ReplacementChangeError::ManagerNotDeclared {
                node_id: node_id.to_owned(),
            },
        })?;
    if *declared != base.node {
        return Err(ReplacementChangeError::StaleBase);
    }

    let committed = site
        .execution_targets
        .iter()
        .filter(|target| target.site_node_id == node_id)
        .collect::<Vec<_>>();
    if committed.len() != base.targets.len()
        || base
            .targets
            .iter()
            .any(|target| !committed.contains(&target))
    {
        return Err(ReplacementChangeError::StaleBase);
    }

    // The review ran the guard on an earlier read of the site; a deployment placed since then is
    // seen only here, under the lock, where nothing else can be placed before the write.
    let impacts = super::deployment_impacts(
        site.deployments,
        site.applications,
        &replacement.classification().guarded_target_ids(),
    );
    if !impacts.is_empty() {
        return Err(ReplacementChangeError::DeploymentGuard { impacts });
    }

    site.execution_targets
        .retain(|target| target.site_node_id != node_id);
    site.execution_targets
        .extend(replacement.targets().iter().cloned());
    Ok(())
}
/// Records `replacement` for `base`'s node through `site`, the authoritative live-site membership
/// write: the change reaches `site.yaml` before the site accepts it, and a site that already
/// declares the replacement writes nothing.
pub(crate) fn record_replacement(
    site: &SiteRepository,
    base: &RescanBase,
    replacement: &RescanReplacement,
    role: ReplacementRole,
) -> Result<(), RescanCommitError> {
    site.mutate_membership(
        |site| replace_targets(site, base, replacement, role),
        |()| {},
    )
    .map_err(commit_error)
}

/// Projects a refused or failed replacement write. An uncertain storage error means `site.yaml` may
/// hold the replacement; any other means the site holds the declaration it held before.
fn commit_error(error: ScopedSiteMutationError<ReplacementChangeError>) -> RescanCommitError {
    match error {
        ScopedSiteMutationError::Owner(error) => {
            RescanCommitError::Rejected(anyhow::Error::new(error))
        }
        ScopedSiteMutationError::Repository(
            error @ SiteRepositoryError::Storage(SiteStorageError::OutcomeUncertain(_)),
        ) => RescanCommitError::OutcomeUncertain(anyhow::Error::new(error)),
        ScopedSiteMutationError::Repository(
            error @ (SiteRepositoryError::InvalidSite(_)
            | SiteRepositoryError::NodeViewLimitExceeded { .. }),
        ) => RescanCommitError::Rejected(anyhow::Error::new(error)),
        ScopedSiteMutationError::Repository(error) => {
            RescanCommitError::Failed(anyhow::Error::new(error))
        }
    }
}
