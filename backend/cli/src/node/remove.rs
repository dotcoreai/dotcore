//! `dotc node remove`: takes one worker node out of a stopped site, for example before its
//! machine gets a new address and joins again.
//!
//! The command holds the site exclusively from planning through the write, shows what the removal
//! changes, and asks for confirmation. A node that still has deployments is refused unless the
//! operator chose to forget them, which also deletes the routes only those deployments serve.

use anyhow::Context;
use clap::Args;
use dotcore::model::engine::declaration_validator;
use dotcore_proto::dotcore::site::v1::{ExecutionTarget, execution_target::RuntimeBinding};
use dotcore_site::{
    initial::validation::SiteInputBundleValidator,
    membership::{NodeRemoval, NodeRemovalPlan, OfflineSite},
};

use super::{Notices, boxed, record_value, write_records};
use crate::{
    RootPathArgs,
    terminal::{
        ConfirmPrompt, Message, NextStep, PromptError, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// Arguments of `dotc node remove`.
#[derive(Debug, Args)]
pub struct NodeRemoveArgs {
    /// Display name or node id of the worker node to remove.
    #[arg(value_name = "NODE")]
    pub node: String,

    /// Drop the declarations of deployments placed on the node, and the routes only they serve,
    /// without cleanup evidence from the node. Without it, a node with deployments is refused.
    #[arg(long)]
    pub forget_deployments: bool,

    /// Remove the node without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Removes the node from the stopped site after the operator confirms.
pub(crate) fn node_remove(paths: &RootPathArgs, arguments: NodeRemoveArgs) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    if !arguments.yes && controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!(
            "removing a node needs a terminal; run in a terminal or pass --yes to remove it without confirmation"
        );
    }

    let site = OfflineSite::open(
        &site_dir,
        &SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore::model::site_declaration_rules(),
        ),
    )
    .map_err(|error| {
        anyhow::Error::new(error).context(format!(
            "cannot remove a node from the site at {}",
            site_dir.display()
        ))
    })?;
    let plan = match site
        .plan_node_removal(&arguments.node, arguments.forget_deployments)
        .with_context(|| format!("cannot remove node {:?}", arguments.node))?
    {
        NodeRemoval::Planned(plan) => plan,
        NodeRemoval::AlreadyRemoved { node_id } => {
            let mut terminal = (!arguments.yes).then(open_terminal);
            return report_already_removed(boxed(&mut terminal), &node_id);
        }
    };

    let mut terminal = (!arguments.yes).then(open_terminal);
    if let Some(terminal) = boxed(&mut terminal) {
        review_plan(terminal, &plan)?;
        if !terminal.confirm(&ConfirmPrompt::new(
            "Remove this node from the site?",
            false,
        ))? {
            return Err(PromptError::Cancelled.into());
        }
    }

    site.remove_node(&plan)
        .with_context(|| format!("cannot remove node {:?}", plan.node.display_name))?;
    drop(site);
    report_removed(boxed(&mut terminal), &plan)
}

/// Shows the node, its execution targets, and every declaration the removal drops.
fn review_plan(terminal: &mut dyn Terminal, plan: &NodeRemovalPlan) -> anyhow::Result<()> {
    terminal.message(&Message::Heading(format!(
        "Remove {} from the site",
        plan.node.display_name
    )))?;
    terminal.message(&Message::KeyValues(vec![
        ("host".to_owned(), plan.node.host.clone()),
        (
            "targets".to_owned(),
            target_summary(&plan.execution_targets),
        ),
    ]))?;
    if !plan.forgotten_deployments.is_empty() {
        terminal.message(&Message::Warning(
            "These deployments are dropped without cleaning up the node".to_owned(),
        ))?;
        terminal.message(&Message::KeyValues(
            plan.forgotten_deployments
                .iter()
                .map(|deployment| {
                    (
                        deployment.kind.to_string(),
                        format!("{} ({})", deployment.name, deployment.state),
                    )
                })
                .collect(),
        ))?;
    }
    if !plan.deleted_routes.is_empty() {
        terminal.message(&Message::Warning(
            "These routes only served those deployments and are deleted".to_owned(),
        ))?;
        terminal.message(&Message::KeyValues(
            plan.deleted_routes
                .iter()
                .map(|route| ("route".to_owned(), route.name.clone()))
                .collect(),
        ))?;
    }
    Ok(())
}

/// Counts the execution targets by kind, for example `1 model, 1 application`.
fn target_summary(targets: &[ExecutionTarget]) -> String {
    let count = |model| {
        targets
            .iter()
            .filter(|target| {
                matches!(
                    (&target.runtime_binding, model),
                    (Some(RuntimeBinding::ModelRuntime(_)), true)
                        | (Some(RuntimeBinding::ApplicationRuntime(_)), false)
                )
            })
            .count()
    };
    match (count(true), count(false)) {
        (0, 0) => "none".to_owned(),
        (models, 0) => format!("{models} model"),
        (0, applications) => format!("{applications} application"),
        (models, applications) => format!("{models} model, {applications} application"),
    }
}

/// Reports the removal, on the terminal when interactive, else as records.
fn report_removed(
    terminal: Option<&mut dyn Terminal>,
    plan: &NodeRemovalPlan,
) -> anyhow::Result<()> {
    let node = &plan.node;
    let mut notices = Notices::new(terminal);
    if notices.terminal.is_some() {
        notices.success(&format!("Removed {}", node.display_name))?;
    } else {
        let mut records = format!(
            "Node removed\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_NAME={}\n",
            record_value(&node.node_id)?,
            record_value(&node.display_name)?,
        );
        for deployment in &plan.forgotten_deployments {
            records.push_str(&format!(
                "DOTCORE_FORGOTTEN_DEPLOYMENT={}\n",
                record_value(&deployment.id)?
            ));
        }
        for route in &plan.deleted_routes {
            records.push_str(&format!(
                "DOTCORE_DELETED_ROUTE={}\n",
                record_value(&route.id)?
            ));
        }
        write_records(&records)?;
    }
    notices.instruct(
        vec![
            NextStep::new("On that machine, stop its server, then run:")
                .with_command("dotc node leave"),
        ],
        "Stop the removed node's server, then run `dotc node leave` on that machine to stop its workloads and delete its node folder.",
    )
}

/// Reports a removal an earlier run already finished.
fn report_already_removed(
    terminal: Option<&mut dyn Terminal>,
    node_id: &str,
) -> anyhow::Result<()> {
    match terminal {
        Some(terminal) => {
            terminal.message(&Message::Success(format!(
                "{node_id} is already removed from the site"
            )))?;
            Ok(())
        }
        None => write_records(&format!(
            "Node already removed\nDOTCORE_NODE_ID={}\n",
            record_value(node_id)?
        )),
    }
}
