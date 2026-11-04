//! A stopped site opened for offline membership changes: a node joins through it and a worker
//! node is removed through it. It holds the exclusive site lock from opening until it
//! is dropped, reads `site.yaml` from disk, and loads the site signing key only to sign an
//! approved node.
//!
//! Opening it fails while the manager runs, so the stopped-site rule and the signing-key custody
//! rule both hold.

use std::path::{Path, PathBuf};

use dotcore_proto::dotcore::site::v1::{ExecutionTarget, Site, SiteNode};

use super::join::{
    JoinHostIdentity, JoinSiteError, JoinSiteWriter, publish_node, read_node_pin,
    replace_node_hosts,
};
use crate::{
    SITE_FILE,
    initial::{
        input_store::{SiteInputLock, SiteInputLockError, try_acquire_exclusive_site_lock},
        validation::{SiteInputBundleValidator, SiteInputValidationError},
    },
    manager_trust::{ManagerTrust, ManagerTrustError, validate_for_private_startup},
    node::folder::CertificateFingerprint,
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, load_validated_site},
};

/// A stopped site opened for one membership change.
#[derive(Debug)]
pub struct OfflineSite {
    pub(super) site_root: PathBuf,
    pub(super) lock: SiteInputLock,

    /// Owner rules every read and written site must satisfy.
    pub(super) rules: SiteDeclarationRules,
}

impl OfflineSite {
    /// Takes the exclusive site lock and checks the site inputs with the given validator. Fails
    /// while the manager runs.
    pub fn open(
        site_root: &Path,
        validator: &SiteInputBundleValidator,
    ) -> Result<Self, OfflineSiteError> {
        let lock = try_acquire_exclusive_site_lock(site_root)?.ok_or_else(|| {
            OfflineSiteError::SiteLocked {
                site_root: site_root.to_owned(),
            }
        })?;
        validator
            .validate_without_runtime_declarations(site_root)
            .map_err(OfflineSiteError::InvalidSite)?;
        Ok(Self {
            site_root: site_root.to_owned(),
            rules: validator.declaration_rules(),
            lock,
        })
    }

    /// Loads the manager's node identity for the join listener.
    pub fn host_identity(&self) -> Result<JoinHostIdentity, OfflineSiteError> {
        let site = self.load_site().map_err(OfflineSiteError::UnreadableSite)?;
        let manager = site
            .nodes
            .iter()
            .find(|node| node.site_manager.is_some())
            .ok_or_else(|| {
                OfflineSiteError::ManagerIdentity(anyhow::anyhow!(
                    "site.yaml declares no site manager"
                ))
            })?;
        let identity = validate_for_private_startup(&self.site_root, &site)
            .map_err(OfflineSiteError::ManagerIdentity)?;
        Ok(JoinHostIdentity::new(
            &site.site_id,
            &manager.node_id,
            identity.trust_anchor,
            identity.certificate,
            identity.private_key,
        ))
    }

    pub(super) fn load_site(&self) -> Result<Site, SiteRepositoryError> {
        load_validated_site(&self.site_root.join(SITE_FILE), &self.rules)
    }
}

impl JoinSiteWriter for OfflineSite {
    fn declaration_rules(&self) -> SiteDeclarationRules {
        self.rules
    }

    fn site(&self) -> Result<Site, JoinSiteError> {
        self.load_site()
            .map_err(|source| JoinSiteError::Failed(anyhow::Error::new(source)))
    }

    fn sign_node_certificate(
        &self,
        site: &Site,
        node: &SiteNode,
        csr_der: &[u8],
    ) -> Result<Vec<u8>, JoinSiteError> {
        ManagerTrust::load(&self.site_root, site)
            .and_then(|trust| trust.sign_csr(&site.site_id, &node.node_id, csr_der))
            .map_err(|error| match error {
                ManagerTrustError::InvalidCsr(_) => {
                    JoinSiteError::Rejected(anyhow::Error::new(error))
                }
                ManagerTrustError::Issuance(_)
                | ManagerTrustError::Invalid(_)
                | ManagerTrustError::Storage(_) => JoinSiteError::Failed(anyhow::Error::new(error)),
            })
    }

    fn publish_node(
        &self,
        node: &SiteNode,
        targets: &[ExecutionTarget],
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        publish_node(
            &self.site_root,
            &self.lock,
            &self.rules,
            node,
            targets,
            certificate_der,
        )?;
        Ok(())
    }

    fn node_pin(&self, node_id: &str) -> Result<Option<CertificateFingerprint>, JoinSiteError> {
        Ok(read_node_pin(&self.site_root, node_id)?)
    }

    fn replace_node_hosts(
        &self,
        node: &SiteNode,
        certificate_der: &[u8],
    ) -> Result<(), JoinSiteError> {
        replace_node_hosts(
            &self.site_root,
            &self.lock,
            &self.rules,
            node,
            certificate_der,
        )?;
        Ok(())
    }
}

/// The stopped site cannot be opened for a membership change.
#[derive(Debug, thiserror::Error)]
pub enum OfflineSiteError {
    #[error("site at {site_root} is running or locked; stop the manager and retry")]
    SiteLocked { site_root: PathBuf },

    #[error("site inputs are invalid: {0}")]
    InvalidSite(#[source] SiteInputValidationError),

    #[error("site.yaml could not be read: {0}")]
    UnreadableSite(#[source] SiteRepositoryError),

    #[error("the manager node identity is invalid: {0}")]
    ManagerIdentity(#[source] anyhow::Error),

    #[error(transparent)]
    Lock(#[from] SiteInputLockError),
}
