//! `dotc node leave`: run on a joined machine whose node left its site, it stops the workloads
//! that belong to this node, Docker containers and native processes alike, and deletes the node
//! folder.
//!
//! Docker cleanup is limited to this node's own execution targets, so a manager or another node on
//! the same Docker daemon keeps its workloads; native workloads are the ones the node folder holds.
//! The command refuses while the node's server runs and never contacts the manager.

use std::sync::Arc;

use anyhow::Context;
use clap::Args;
use dotcore_site::{
    membership::{NodeLeave, NodeLeaveError},
    reset::persistence::SitePersistenceReset,
};

use serde::Serialize;

use super::Notices;
use crate::{
    RootPathArgs,
    output::{LazyTerminal, OutputFormat, require_consent, write_yaml},
    runtime_confirmation::{
        RuntimeRemovalApproval, approve_runtime_removal, site_runtime_cleanups,
    },
    terminal::{ConfirmPrompt, Message, PromptError, Terminal},
};

/// Arguments of `dotc node leave`.
#[derive(Debug, Args)]
pub struct NodeLeaveArgs {
    /// Stop the node's workloads and delete its folder without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Stops this node's workloads and deletes its node folder after the operator confirms.
pub(crate) async fn node_leave(
    paths: &RootPathArgs,
    arguments: NodeLeaveArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    require_consent(
        arguments.yes,
        format,
        "leaving the site needs a terminal; run in a terminal or pass --yes to stop the node's workloads and delete its folder without confirmation",
        "leaving the site needs confirmation, which --yaml never asks for; pass --yes to stop the node's workloads and delete its folder",
    )?;

    let mut terminal = LazyTerminal::new();
    let cleanups = site_runtime_cleanups(None);
    let leave = NodeLeave::open(&site_dir, Arc::clone(&cleanups))
        .with_context(|| format!("cannot leave the site from {}", site_dir.display()))?;
    let runtime = leave
        .runtime_cleanup()
        .context("select the runtime cleanup for the node's execution targets")?;
    let inventory = leave.inventory(runtime.as_ref()).await.map_err(|error| {
        leave_failure(
            error,
            "list the runtime resources of the node".to_owned(),
            format!(
                "cannot leave the site from {}: its native workloads could not be listed safely, so nothing was stopped and the node folder was kept; resolve the cause below and retry",
                site_dir.display()
            ),
        )
    })?;

    if !arguments.yes {
        review(&mut terminal, &leave)?;
    }
    if inventory.resources.is_empty()
        && !arguments.yes
        && !terminal.confirm(&ConfirmPrompt::new("Delete the node folder?", false))?
    {
        return Err(PromptError::Cancelled.into());
    }
    approve_runtime_removal(
        &mut terminal,
        &site_dir,
        &inventory,
        &RuntimeRemovalApproval {
            subject: format!("Node at {}", site_dir.display()),
            verb: "leave the site",
            question: "Stop these resources and delete the node folder?",
            yes: arguments.yes,
            non_interactive: false,
            skip_runtime_cleanup: false,
        },
    )?;

    let site_id = leave.site_id().to_owned();
    let node_id = leave.node_id().to_owned();
    leave
        .leave(runtime.as_ref(), &SitePersistenceReset::new(cleanups))
        .await
        .map_err(|error| {
            leave_failure(
                error,
                format!(
                    "node leave failed for {}; fix the cause and run it again",
                    site_dir.display()
                ),
                format!(
                    "cannot leave the site from {}: its native workloads could not be stopped and removed, so the node folder was kept; resolve the cause below and retry",
                    site_dir.display()
                ),
            )
        })?;

    if format == OutputFormat::Yaml {
        write_yaml(&LeftDocument {
            outcome: "left",
            site_id,
            node_id,
        })
    } else {
        Notices::new(Some(&mut terminal)).success(&format!("Left {site_id}"))
    }
}

/// The YAML document of a node that left its site.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LeftDocument {
    outcome: &'static str,
    site_id: String,
    node_id: String,
}

/// Shows which node leaves, and what leaving will stop and delete.
fn review(terminal: &mut dyn Terminal, leave: &NodeLeave) -> anyhow::Result<()> {
    terminal.prompt_context(&Message::Heading(format!(
        "This node leaves site {}",
        leave.site_id()
    )))?;
    terminal.prompt_context(&Message::KeyValues(vec![
        (
            "targets".to_owned(),
            leave.execution_target_ids().len().to_string(),
        ),
        (
            "node folder".to_owned(),
            format!("{} (deleted)", leave.node_root().display()),
        ),
    ]))?;
    Ok(())
}

/// Adds `context` to a failed leave step, or `native_context` when the failure is in the node's
/// native workloads, which says what the step left behind so the command can be retried.
fn leave_failure(error: NodeLeaveError, context: String, native_context: String) -> anyhow::Error {
    let context = match &error {
        NodeLeaveError::NativeCleanup { .. } => native_context,
        _ => context,
    };
    anyhow::Error::new(error).context(context)
}
