//! Lists the nodes a site declares, for example the manager and one GPU
//! worker with their addresses, execution targets, and placed deployments.
//!
//! The listing reads `site.yaml` and the pin folder without taking the site lock, so it works
//! whether the manager runs or is stopped: every file it reads is replaced atomically. It
//! refuses a joined node folder, whose site lives on the manager.

use std::{
    io,
    path::{Path, PathBuf},
};

use dotcore_proto::dotcore::site::v1::{ExecutionTarget, SiteNode, execution_target};

use crate::{
    SITE_FILE,
    manager_trust::{SITE_TRUST_DIRECTORY, WORKER_PIN_DIRECTORY},
    node::folder::WORKER_MANIFEST_FILE,
    repository::SiteRepositoryError,
    validation::{SiteDeclarationRules, load_validated_site},
};

/// The nodes of one site, manager first as declared.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteNodeListing {
    /// Identity of the listed site.
    pub site_id: String,

    /// Every declared node in `site.yaml` order.
    pub nodes: Vec<ListedNode>,
}

/// One declared node with the facts an operator checks before changing membership.
#[derive(Clone, Debug, PartialEq)]
pub struct ListedNode {
    /// The declared node.
    pub node: SiteNode,

    /// Whether the node is the site manager.
    pub is_manager: bool,

    /// The execution targets the node provides.
    pub execution_targets: Vec<ListedTarget>,

    /// Whether the manager holds a pin for the node's certificate. The manager itself has none.
    pub pinned: bool,

    /// Number of model deployment declarations placed on the node.
    pub placed_models: usize,

    /// Number of application deployment declarations placed on the node.
    pub placed_applications: usize,
}

/// An execution target as the listing shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedTarget {
    pub id: String,

    /// Human-readable name, unique among the node's targets.
    pub display_name: String,

    /// `model`, `application`, or `unbound`.
    pub kind: &'static str,

    /// Platform as `os/architecture`, with the variant appended when declared.
    pub platform: String,
}

/// Reads the nodes of the manager site folder at `site_root`.
pub fn list_site_nodes(
    site_root: &Path,
    rules: &SiteDeclarationRules,
) -> Result<SiteNodeListing, NodeListError> {
    let site_file = site_root.join(SITE_FILE);
    if !site_file.is_file() {
        return Err(if site_root.join(WORKER_MANIFEST_FILE).is_file() {
            NodeListError::JoinedNodeFolder {
                node_root: site_root.to_owned(),
            }
        } else {
            NodeListError::NotASiteFolder {
                site_root: site_root.to_owned(),
            }
        });
    }
    let site = load_validated_site(&site_file, rules).map_err(NodeListError::SiteUnreadable)?;

    let pins = site_root
        .join(SITE_TRUST_DIRECTORY)
        .join(WORKER_PIN_DIRECTORY);
    let nodes = site
        .nodes
        .iter()
        .map(|node| {
            let pin = pins.join(format!("{}.sha256", node.node_id));
            let pinned = pin.try_exists().map_err(|source| NodeListError::Pin {
                path: pin.clone(),
                source,
            })?;
            Ok(ListedNode {
                node: node.clone(),
                is_manager: node.site_manager.is_some(),
                execution_targets: site
                    .execution_targets
                    .iter()
                    .filter(|target| target.site_node_id == node.node_id)
                    .map(listed_target)
                    .collect(),
                pinned,
                placed_models: site
                    .model_deployments
                    .iter()
                    .filter(|deployment| placed_on(deployment.placement.as_ref(), &node.node_id))
                    .count(),
                placed_applications: site
                    .application_deployments
                    .iter()
                    .filter(|deployment| placed_on(deployment.placement.as_ref(), &node.node_id))
                    .count(),
            })
        })
        .collect::<Result<Vec<_>, NodeListError>>()?;
    Ok(SiteNodeListing {
        site_id: site.site_id,
        nodes,
    })
}

/// The nodes of a site could not be listed.
#[derive(Debug, thiserror::Error)]
pub enum NodeListError {
    /// The folder is a joined node folder, which does not hold the site.
    #[error("{node_root} is a joined node folder; the node list lives on the manager")]
    JoinedNodeFolder {
        /// Folder that was opened.
        node_root: PathBuf,
    },

    /// The folder holds neither a site nor a joined node.
    #[error("{site_root} holds no site (no site.yaml)")]
    NotASiteFolder {
        /// Folder that was opened.
        site_root: PathBuf,
    },

    /// `site.yaml` could not be read or is invalid.
    #[error("site.yaml could not be read: {0}")]
    SiteUnreadable(#[source] SiteRepositoryError),

    /// A node's pin could not be inspected.
    #[error("inspect the node pin {path}: {source}")]
    Pin {
        /// Pin file that could not be inspected.
        path: PathBuf,

        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },
}

fn listed_target(target: &ExecutionTarget) -> ListedTarget {
    let kind = match target.runtime_binding {
        Some(execution_target::RuntimeBinding::ModelRuntime(_)) => "model",
        Some(execution_target::RuntimeBinding::ApplicationRuntime(_)) => "application",
        None => "unbound",
    };
    let platform = target
        .platform
        .as_ref()
        .map_or_else(String::new, |platform| match &platform.variant {
            Some(variant) => format!(
                "{}/{}/{variant}",
                platform.operating_system, platform.architecture
            ),
            None => format!("{}/{}", platform.operating_system, platform.architecture),
        });
    ListedTarget {
        id: target.execution_target_id.clone(),
        display_name: target.display_name.clone(),
        kind,
        platform,
    }
}

fn placed_on(
    placement: Option<&dotcore_proto::dotcore::deployment::v1::ExactPlacement>,
    node_id: &str,
) -> bool {
    placement.is_some_and(|placement| placement.site_node_id == node_id)
}
