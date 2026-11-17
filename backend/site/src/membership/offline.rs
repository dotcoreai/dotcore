//! A stopped site opened for offline membership changes, such as removing a worker node. It
//! holds the exclusive site lock from opening until it is dropped and reads `site.yaml` from
//! disk.
//!
//! Opening it fails while the manager runs, so removal only ever changes a stopped site.

use std::path::{Path, PathBuf};

use dotcore_proto::dotcore::site::v1::Site;

use crate::{
    SITE_FILE,
    initial::{
        input_store::{SiteInputLock, SiteInputLockError, try_acquire_exclusive_site_lock},
        validation::{SiteInputBundleValidator, SiteInputValidationError},
    },
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

    pub(super) fn load_site(&self) -> Result<Site, SiteRepositoryError> {
        load_validated_site(&self.site_root.join(SITE_FILE), &self.rules)
    }
}

/// The stopped site cannot be opened for a membership change.
#[derive(Debug, thiserror::Error)]
pub enum OfflineSiteError {
    #[error("site at {site_root} is running or locked; stop the manager and retry")]
    SiteLocked { site_root: PathBuf },

    #[error("site inputs are invalid: {0}")]
    InvalidSite(#[source] SiteInputValidationError),

    #[error(transparent)]
    Lock(#[from] SiteInputLockError),
}
