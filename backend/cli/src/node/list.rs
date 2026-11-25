//! `dotc node list`: shows the nodes a site declares, with their addresses, execution targets,
//! and placed deployments, for example the manager and one GPU worker.
//!
//! It reads the site without locking it, so it works while the manager runs. It prints a table,
//! with optional detail columns and execution targets, on the terminal or as plain text without
//! one. Asked for YAML, it prints the site's nodes with every fact, whatever the detail flags.

use anyhow::Context;
use clap::Args;
use dotcore_proto::dotcore::site::v1::SiteNode;
use dotcore_site::membership::{ListedNode, ListedTarget, list_site_nodes};
use serde::Serialize;

use crate::{
    RootPathArgs,
    output::{OutputFormat, present, write_yaml},
    terminal::Message,
};

/// Arguments of `dotc node list`.
#[derive(Debug, Args)]
pub struct NodeListArgs {
    /// Also show the address each node uses to reach the manager and whether the manager trusts
    /// the node's certificate.
    #[arg(long)]
    pub details: bool,

    /// Also list each node's execution targets.
    #[arg(long)]
    pub execution_targets: bool,
}

/// Lists the site's nodes as YAML when asked, else as a table.
pub(crate) fn node_list(
    paths: &RootPathArgs,
    arguments: NodeListArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    let listing = list_site_nodes(&site_dir, &dotcore::model::site_declaration_rules())
        .with_context(|| format!("cannot list the nodes of {}", site_dir.display()))?;

    if format == OutputFormat::Yaml {
        return write_yaml(&NodeListDocument::of(&listing.site_id, &listing.nodes));
    }

    let mut messages = vec![
        Message::Heading(format!("Nodes of site {}", listing.site_id)),
        table(&listing.nodes, arguments.details),
    ];
    let targets = if arguments.execution_targets {
        target_rows(&listing.nodes)
    } else {
        Vec::new()
    };
    if !targets.is_empty() {
        messages.push(Message::Heading("Execution targets".to_owned()));
        messages.push(Message::KeyValues(targets));
    }
    present(&messages)
}

/// The site's nodes read from its folder: each declared node as the site declares it, plus what
/// the manager's folder knows about it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeListDocument<'a> {
    site_id: &'a str,
    nodes: Vec<NodeDocument<'a>>,
}

/// One declared node with its role, trust and placements.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NodeDocument<'a> {
    node: &'a SiteNode,
    is_manager: bool,

    /// Whether the manager holds a pin for the node's certificate; the manager has none.
    pinned: bool,
    execution_targets: Vec<TargetDocument<'a>>,
    placed_models: usize,
    placed_applications: usize,
}

/// One execution target a node provides.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetDocument<'a> {
    id: &'a str,
    display_name: &'a str,
    kind: &'static str,
    platform: &'a str,
}

impl<'a> NodeListDocument<'a> {
    fn of(site_id: &'a str, nodes: &'a [ListedNode]) -> Self {
        Self {
            site_id,
            nodes: nodes
                .iter()
                .map(|listed| NodeDocument {
                    node: &listed.node,
                    is_manager: listed.is_manager,
                    pinned: listed.pinned,
                    execution_targets: listed.execution_targets.iter().map(target).collect(),
                    placed_models: listed.placed_models,
                    placed_applications: listed.placed_applications,
                })
                .collect(),
        }
    }
}

fn target(target: &ListedTarget) -> TargetDocument<'_> {
    TargetDocument {
        id: &target.id,
        display_name: &target.display_name,
        kind: target.kind,
        platform: &target.platform,
    }
}

/// Builds the nodes table, header first. `details` adds the manager address and trust columns.
fn table(nodes: &[ListedNode], details: bool) -> Message {
    let mut header = vec!["NAME", "ID", "ROLE", "HOST"];
    if details {
        header.extend(["REACHES MANAGER AT", "TRUSTED"]);
    }
    header.extend(["MODELS", "APPS"]);

    let rows = nodes
        .iter()
        .map(|listed| {
            let mut row = vec![
                listed.node.display_name.clone(),
                listed.node.node_id.clone(),
                role(listed).to_owned(),
                listed.node.host.clone(),
            ];
            if details {
                row.push(manager_address(listed).to_owned());
                row.push(trusted(listed).to_owned());
            }
            row.push(listed.placed_models.to_string());
            row.push(listed.placed_applications.to_string());
            row
        })
        .collect();

    Message::Table {
        header: header.into_iter().map(str::to_owned).collect(),
        rows,
    }
}

/// Builds one row per execution target, labeled by node: the target's name, then its kind,
/// platform and id.
fn target_rows(nodes: &[ListedNode]) -> Vec<(String, String)> {
    nodes
        .iter()
        .flat_map(|listed| {
            listed.execution_targets.iter().map(|target| {
                (
                    listed.node.display_name.clone(),
                    format!(
                        "{} ({}, {}, {})",
                        target.display_name, target.kind, target.platform, target.id
                    ),
                )
            })
        })
        .collect()
}

fn role(listed: &ListedNode) -> &'static str {
    if listed.is_manager {
        "manager"
    } else {
        "worker"
    }
}

/// The address the node uses to reach the manager, `-` when it uses the manager's declared host.
fn manager_address(listed: &ListedNode) -> &str {
    if listed.node.manager_host.is_empty() {
        "-"
    } else {
        &listed.node.manager_host
    }
}

/// Whether the manager trusts the worker's certificate; the manager node has none to trust.
fn trusted(listed: &ListedNode) -> &'static str {
    match (listed.is_manager, listed.pinned) {
        (true, _) => "-",
        (false, true) => "yes",
        (false, false) => "no",
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::site::v1::SiteNode;
    use dotcore_site::membership::ListedTarget;

    use super::*;

    fn listed(
        name: &str,
        id: &str,
        host: &str,
        manager_host: &str,
        is_manager: bool,
        pinned: bool,
    ) -> ListedNode {
        ListedNode {
            node: SiteNode {
                node_id: id.to_owned(),
                display_name: name.to_owned(),
                host: host.to_owned(),
                manager_host: manager_host.to_owned(),
                ..Default::default()
            },
            is_manager,
            execution_targets: Vec::new(),
            pinned,
            placed_models: 1,
            placed_applications: 2,
        }
    }

    fn nodes() -> Vec<ListedNode> {
        vec![
            listed("gpu1", "node-1", "desk.example", "", true, false),
            listed("spark", "node-2", "spark.example", "10.0.0.1", false, true),
            listed("lost", "node-3", "lost.example", "", false, false),
        ]
    }

    #[test]
    fn default_table_shows_only_the_daily_columns() {
        assert_eq!(
            table(&nodes(), false).plain_lines(),
            vec![
                "  NAME   ID      ROLE     HOST           MODELS  APPS",
                "  gpu1   node-1  manager  desk.example   1       2",
                "  spark  node-2  worker   spark.example  1       2",
                "  lost   node-3  worker   lost.example   1       2",
            ]
        );
    }

    #[test]
    fn details_table_adds_manager_address_and_trust() {
        assert_eq!(
            table(&nodes(), true).plain_lines(),
            vec![
                "  NAME   ID      ROLE     HOST           REACHES MANAGER AT  TRUSTED  MODELS  APPS",
                "  gpu1   node-1  manager  desk.example   -                   -        1       2",
                "  spark  node-2  worker   spark.example  10.0.0.1            yes      1       2",
                "  lost   node-3  worker   lost.example   -                   no       1       2",
            ]
        );
    }

    #[test]
    fn target_rows_show_the_name_with_kind_platform_and_id() {
        let mut spark = listed("spark", "node-2", "spark.example", "", false, true);
        spark.execution_targets = vec![
            ListedTarget {
                id: "execution-target-0a1b".to_owned(),
                display_name: "2\u{d7} NVIDIA GeForce RTX 5090".to_owned(),
                kind: "model",
                platform: "linux/arm64".to_owned(),
            },
            ListedTarget {
                id: "application-execution-target-0a1b".to_owned(),
                display_name: "Docker applications".to_owned(),
                kind: "application",
                platform: "linux/arm64".to_owned(),
            },
        ];

        assert_eq!(
            target_rows(&[spark]),
            vec![
                (
                    "spark".to_owned(),
                    "2\u{d7} NVIDIA GeForce RTX 5090 (model, linux/arm64, execution-target-0a1b)"
                        .to_owned()
                ),
                (
                    "spark".to_owned(),
                    "Docker applications (application, linux/arm64, application-execution-target-0a1b)"
                        .to_owned()
                ),
            ]
        );
    }
}
