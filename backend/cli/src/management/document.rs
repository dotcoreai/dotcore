//! The YAML documents of the deployment read commands: each deployment as the manager returns it,
//! plus the names of the node and execution target it runs on.
//!
//! A document never reshapes the manager's message: it keeps the message's protobuf JSON mapping
//! and only adds the display names the CLI resolves, so a script reads the API's own fields.

use serde::Serialize;

use super::{ManagedDeployment, NodeNames};

/// One deployment message with the display names of where it runs, when the manager named them.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeploymentDocument<T> {
    #[serde(flatten)]
    deployment: T,

    #[serde(skip_serializing_if = "Option::is_none")]
    site_node_display_name: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    execution_target_display_name: Option<String>,
}

impl<T: ManagedDeployment> DeploymentDocument<T> {
    /// Wraps a deployment with the names of its placement, if it has one.
    pub(crate) fn new(deployment: T, nodes: &NodeNames) -> Self {
        let placement = deployment.placement();
        Self {
            site_node_display_name: placement
                .and_then(|placement| nodes.name(&placement.site_node_id))
                .map(str::to_owned),
            execution_target_display_name: placement
                .and_then(|placement| {
                    nodes.target_name(&placement.site_node_id, &placement.execution_target_id)
                })
                .map(str::to_owned),
            deployment,
        }
    }
}

/// The deployments of one kind, shaped as the manager's list response.
#[derive(Debug, Serialize)]
pub(crate) struct DeploymentListDocument<T> {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    deployments: Vec<DeploymentDocument<T>>,
}

impl<T> DeploymentListDocument<T> {
    /// Lists the documents in the manager's order.
    pub(crate) fn new(deployments: Vec<DeploymentDocument<T>>) -> Self {
        Self { deployments }
    }
}
