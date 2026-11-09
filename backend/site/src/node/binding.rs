//! Validation of one server-local node binding against declared site membership.
//!
//! The binding parses the configured identity once and returns it together with the
//! declaration it matched, so callers carry a checked identity such as `node-a`
//! instead of rebuilding one from the declaration text.

use dotcore_domain::{SiteNodeId, SiteNodeIdError};
use dotcore_node_hardware::{MachineDescription, MachineError};
use dotcore_proto::dotcore::{
    node::v1::NodeConfiguration,
    site::v1::{Site, SiteNode},
};

/// Default number of daily backend log files to retain.
pub const DEFAULT_LOG_RETENTION_DAYS: u32 = 7;

/// A server-local binding to exactly one declared SiteNode.
///
/// It carries the identity parsed once from `node.yaml` alongside the site
/// declaration that identity matched, so callers never rebuild domain identity
/// from a declaration string.
#[derive(Clone, Debug)]
pub struct BoundSiteNode {
    /// Identity parsed from `node.yaml`.
    pub node_id: SiteNodeId,

    /// Declared site membership matched by the parsed identity.
    pub declaration: SiteNode,

    /// Machine the node declares in place of its real hardware, if any.
    pub declared_machine: Option<MachineDescription>,
}

/// Validates and resolves the server-local backend log retention policy.
pub fn logging_retention_days(configuration: &NodeConfiguration) -> Result<u32, NodeBindingError> {
    let logging_retention_days = configuration
        .logging
        .as_ref()
        .map_or(DEFAULT_LOG_RETENTION_DAYS, |policy| policy.retention_days);
    if !(1..=365).contains(&logging_retention_days) {
        return Err(NodeBindingError::InvalidRetentionDays {
            logging_retention_days,
        });
    }
    Ok(logging_retention_days)
}

/// Validates a local node binding and returns its parsed identity with the
/// declared SiteNode it matched and the machine it declares.
pub fn bound_site_node(
    site: &Site,
    configuration: &NodeConfiguration,
) -> Result<BoundSiteNode, NodeBindingError> {
    logging_retention_days(configuration)?;
    let node_id = SiteNodeId::parse(&configuration.node_id).map_err(
        |SiteNodeIdError::Invalid { value }| NodeBindingError::InvalidNodeId { node_id: value },
    )?;
    let matching_nodes = site
        .nodes
        .iter()
        .filter(|node| node.node_id == node_id.as_str())
        .collect::<Vec<_>>();
    match matching_nodes.as_slice() {
        [node] => Ok(BoundSiteNode {
            node_id,
            declaration: (*node).clone(),
            declared_machine: declared_machine(configuration)?,
        }),
        [] => Err(NodeBindingError::MissingSiteNode {
            node_id: node_id.as_str().to_owned(),
        }),
        _ => Err(NodeBindingError::AmbiguousSiteNode {
            node_id: node_id.as_str().to_owned(),
        }),
    }
}

/// Returns the machine the node configuration declares, if any, checked like any description.
fn declared_machine(
    configuration: &NodeConfiguration,
) -> Result<Option<MachineDescription>, NodeBindingError> {
    let Some(declared) = configuration.declared_machine.as_ref() else {
        return Ok(None);
    };
    let description = declared
        .description
        .as_ref()
        .ok_or(NodeBindingError::MissingMachineDescription)?;
    MachineDescription::from_stored(description)
        .map(Some)
        .map_err(NodeBindingError::InvalidMachine)
}

/// A server-local binding did not identify exactly one declared SiteNode.
#[derive(Debug, thiserror::Error)]
pub enum NodeBindingError {
    /// The configured log retention is outside the supported range.
    #[error(
        "node.yaml logging.retentionDays must be between 1 and 365, got {logging_retention_days}"
    )]
    InvalidRetentionDays {
        /// Rejected retention period.
        logging_retention_days: u32,
    },

    /// The node identity does not satisfy the site identifier rule.
    #[error("node.yaml nodeId {node_id:?} must match [a-z][a-z0-9-]{{0,62}}")]
    InvalidNodeId {
        /// Rejected node identity.
        node_id: String,
    },

    /// The node identity is not a declared SiteNode.
    #[error("node.yaml nodeId {node_id:?} does not reference a declared node")]
    MissingSiteNode {
        /// Unmatched node identity.
        node_id: String,
    },

    /// The site has duplicate declarations for the bound identity.
    #[error("node.yaml nodeId {node_id:?} must bind exactly one node")]
    AmbiguousSiteNode {
        /// Ambiguous node identity.
        node_id: String,
    },

    /// The declared machine has no hardware description.
    #[error("node.yaml declaredMachine has no description")]
    MissingMachineDescription,

    /// The declared machine's hardware description breaks a description rule.
    #[error("node.yaml declaredMachine description is invalid")]
    InvalidMachine(#[source] MachineError),
}

#[cfg(test)]
mod tests {
    use dotcore_domain::SiteNodeId;
    use dotcore_node_hardware::{MachineDescription, MachineError};
    use dotcore_proto::dotcore::{
        node::v1::{
            DeclaredMachine, LoggingConfig, MachineDescription as StoredMachine, NodeConfiguration,
        },
        site::v1::{Site, SiteNode},
    };

    use super::{
        DEFAULT_LOG_RETENTION_DAYS, NodeBindingError, bound_site_node, logging_retention_days,
    };

    #[test]
    fn omitted_policy_uses_the_default() {
        assert_eq!(
            logging_retention_days(&NodeConfiguration::default()).expect("default policy"),
            DEFAULT_LOG_RETENTION_DAYS
        );
    }

    #[test]
    fn accepts_policy_boundaries() {
        for days in [1, 365] {
            let configuration = NodeConfiguration {
                logging: Some(LoggingConfig {
                    retention_days: days,
                }),
                ..Default::default()
            };
            assert_eq!(
                logging_retention_days(&configuration).expect("valid policy"),
                days
            );
        }
    }

    #[test]
    fn rejects_policy_outside_boundaries() {
        for days in [0, 366] {
            let configuration = NodeConfiguration {
                logging: Some(LoggingConfig {
                    retention_days: days,
                }),
                ..Default::default()
            };
            assert!(matches!(
                logging_retention_days(&configuration),
                Err(NodeBindingError::InvalidRetentionDays { .. })
            ));
        }
    }
    #[test]
    fn declared_node_identity_is_parsed_once_with_its_declaration() {
        let site = Site {
            nodes: vec![site_node("node-a")],
            ..Default::default()
        };
        let configuration = NodeConfiguration {
            node_id: "node-a".to_owned(),
            ..Default::default()
        };
        let bound = bound_site_node(&site, &configuration).expect("declared node binding");
        assert_eq!(bound.node_id, SiteNodeId::for_test("node-a"));
        assert_eq!(bound.node_id.as_str(), "node-a");
        assert_eq!(bound.declaration, site_node("node-a"));
        assert_eq!(bound.declared_machine, None);
    }

    #[test]
    fn a_declared_machine_binds_its_stored_description_checked_on_load() {
        let site = Site {
            nodes: vec![site_node("node-a")],
            ..Default::default()
        };
        let gb10 = MachineDescription::named("gb10").expect("named machine");
        let declaring = |description: Option<StoredMachine>| NodeConfiguration {
            node_id: "node-a".to_owned(),
            declared_machine: Some(DeclaredMachine {
                choices: vec!["gb10".to_owned()],
                description,
            }),
            ..Default::default()
        };
        let mut duplicate =
            MachineDescription::simulated(&["rtx-5090".to_owned(), "rtx-4090".to_owned()], "amd64")
                .expect("simulated machine")
                .to_stored();
        duplicate.gpus[1].id = duplicate.gpus[0].id.clone();

        let bound = bound_site_node(&site, &declaring(Some(gb10.to_stored())))
            .expect("declared machine binding");
        let missing = bound_site_node(&site, &declaring(None)).expect_err("no description");
        let invalid = bound_site_node(&site, &declaring(Some(duplicate))).expect_err("invalid");

        assert_eq!(bound.declared_machine, Some(gb10));
        assert_eq!(
            missing.to_string(),
            "node.yaml declaredMachine has no description"
        );
        assert_eq!(
            invalid.to_string(),
            "node.yaml declaredMachine description is invalid"
        );
        assert!(matches!(
            invalid,
            NodeBindingError::InvalidMachine(MachineError::DuplicateGpu { .. })
        ));
    }

    #[test]
    fn invalid_node_identity_keeps_the_identifier_rule_error() {
        let site = Site {
            nodes: vec![site_node("node-a")],
            ..Default::default()
        };
        let configuration = NodeConfiguration {
            node_id: "Invalid-node".to_owned(),
            ..Default::default()
        };
        let error = bound_site_node(&site, &configuration).expect_err("invalid identity");
        assert_eq!(
            error.to_string(),
            "node.yaml nodeId \"Invalid-node\" must match [a-z][a-z0-9-]{0,62}"
        );
        assert!(
            matches!(error, NodeBindingError::InvalidNodeId { node_id } if node_id == "Invalid-node")
        );
    }

    #[test]
    fn undeclared_node_identity_keeps_the_exact_binding_error() {
        let site = Site {
            nodes: vec![site_node("node-a")],
            ..Default::default()
        };
        let configuration = NodeConfiguration {
            node_id: "node-b".to_owned(),
            ..Default::default()
        };
        let error = bound_site_node(&site, &configuration).expect_err("undeclared identity");
        assert_eq!(
            error.to_string(),
            "node.yaml nodeId \"node-b\" does not reference a declared node"
        );
        assert!(
            matches!(error, NodeBindingError::MissingSiteNode { node_id } if node_id == "node-b")
        );
    }

    #[test]
    fn ambiguous_node_identity_keeps_the_exact_binding_error() {
        let site = Site {
            nodes: vec![site_node("node-a"), site_node("node-a")],
            ..Default::default()
        };
        let configuration = NodeConfiguration {
            node_id: "node-a".to_owned(),
            ..Default::default()
        };
        let error = bound_site_node(&site, &configuration).expect_err("ambiguous identity");
        assert_eq!(
            error.to_string(),
            "node.yaml nodeId \"node-a\" must bind exactly one node"
        );
        assert!(
            matches!(error, NodeBindingError::AmbiguousSiteNode { node_id } if node_id == "node-a")
        );
    }

    fn site_node(node_id: &str) -> SiteNode {
        SiteNode {
            node_id: node_id.to_owned(),
            display_name: node_id.to_owned(),
            ..Default::default()
        }
    }
}
