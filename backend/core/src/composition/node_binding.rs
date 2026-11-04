//! Composition-owned binding of node.yaml to declared SiteNode configuration.

use std::path::Path;

use dotcore_proto::{
    dotcore::{node::v1::NodeConfiguration, site::v1::Site},
    yaml,
};
use dotcore_site::{
    NODE_FILE, SITE_FILE,
    node::binding::{BoundSiteNode, bound_site_node, logging_retention_days},
    validation::decode_site,
};

use super::CompositionError;

/// Loads and validates the server-local log retention policy and SiteNode binding.
pub(super) fn validated_logging_retention_days_for(
    site_root: &Path,
    site: &Site,
) -> Result<u32, CompositionError> {
    let configuration: NodeConfiguration = yaml::from_file(&site_root.join(NODE_FILE))?;
    let logging_retention_days = logging_retention_days(&configuration)
        .map_err(|error| CompositionError::Composition(anyhow::Error::new(error)))?;
    bound_site_node(site, &configuration)
        .map_err(|error| CompositionError::Composition(anyhow::Error::new(error)))?;
    Ok(logging_retention_days)
}

/// Validates that node.yaml names exactly one declared SiteNode.
pub(super) fn node_configuration_for(
    site_root: &Path,
    site: &Site,
) -> Result<BoundSiteNode, CompositionError> {
    let configuration: NodeConfiguration = yaml::from_file(&site_root.join(NODE_FILE))?;
    bound_site_node(site, &configuration)
        .map_err(|error| CompositionError::Composition(anyhow::Error::new(error)))
}

/// Rejects a full-site start for a SiteNode that is not the site manager before anything at the
/// site root is written; those nodes start only from a joined node folder.
///
/// It reads site.yaml as stored, without the lifecycle upgrade that opening owner state applies.
pub(super) fn require_full_site_manager(site_root: &Path) -> Result<(), CompositionError> {
    let site = decode_site(&site_root.join(SITE_FILE))?;
    require_site_manager(&node_configuration_for(site_root, &site)?)
}

fn require_site_manager(node: &BoundSiteNode) -> Result<(), CompositionError> {
    if node.declaration.site_manager.is_some() {
        return Ok(());
    }
    Err(CompositionError::NotSiteManager {
        node_id: node.node_id.to_string(),
    })
}
