//! Composition-owned immutable catalog and desired site loading.

use dotcore_catalog::{Catalog, OciArtifactPolicy};
use dotcore_domain::SitePaths;
use dotcore_site::{SITE_FILE, repository::SiteRepository};
use tracing::{error, info};

use super::CompositionError;

/// Loads the immutable catalog sources used by one backend composition.
pub(super) fn load_catalog(configuration: &SitePaths) -> Result<Catalog, CompositionError> {
    #[cfg(test)]
    {
        let mut counts = super::CATALOG_LOAD_COUNTS
            .lock()
            .expect("Catalog load count lock");
        *counts.entry(configuration.root.clone()).or_default() += 1;
    }
    let master_catalog_root = configuration
        .master_catalog_root
        .as_deref()
        .ok_or_else(|| {
            CompositionError::composition(format!("{} is not set", dotcore_domain::CATALOG_DIR))
        })?;
    let result = Catalog::load_sources_with_policy(
        master_catalog_root,
        &configuration.root.join("catalog"),
        OciArtifactPolicy::Development,
        crate::model::engine::declaration_validator(),
    );
    match &result {
        Ok(catalog) => info!(
            event_kind = "catalog_loaded",
            model_count = catalog.list_model_releases().len(),
            application_count = catalog.list_application_releases().len(),
            "Catalog loaded"
        ),
        Err(error) => error!(
            event_kind = "catalog_load_failed",
            error_kind = "catalog",
            error = %error,
            "Catalog loading failed"
        ),
    }
    Ok(result?)
}

pub(super) fn load_site(configuration: &SitePaths) -> Result<SiteRepository, CompositionError> {
    #[cfg(test)]
    {
        let mut counts = super::SITE_LOAD_COUNTS
            .lock()
            .expect("Site load count lock");
        *counts.entry(configuration.root.clone()).or_default() += 1;
    }
    let result = SiteRepository::load(
        &configuration.root.join(SITE_FILE),
        crate::model::site_declaration_rules(),
    );
    match &result {
        Ok(site) => {
            let snapshot = site.snapshot();
            info!(
                event_kind = "site_configuration_loaded",
                site_root = %configuration.root.display(),
                site_id = snapshot.as_ref().map(|site| site.site_id.as_str()).unwrap_or("unknown"),
                "Site configuration loaded"
            );
        }
        Err(error) => error!(
            event_kind = "site_configuration_load_failed",
            error_kind = "site",
            error = %error,
            "Site configuration loading failed"
        ),
    }
    Ok(result?)
}
