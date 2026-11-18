//! `dotc node remove`: takes one worker node out of its site, for example before its machine is
//! retired or gets a new address and joins again.
//!
//! On a running site the removal goes through the site manager, which then tells the removed
//! machine to stop; on a stopped site the command holds the site exclusively from planning through
//! the write. Both show what the removal changes and ask for confirmation. A node that still has
//! deployments is refused unless the operator chose to forget them, which also deletes the routes
//! only those deployments serve; the running manager forgets them only for a node that is not
//! connected.

use anyhow::Context;
use clap::Args;
use dotcore::model::engine::declaration_validator;
use dotcore_proto::dotcore::site::v1::{
    ExecutionTarget, ListMachinesRequest, MachineRemovalDeployment, MachineRemovalDeploymentKind,
    MachineRemovalPlan, MachineRemovalRefusal, RemoveMachineRequest,
    execution_target::RuntimeBinding, remove_machine_response,
};
use dotcore_site::{
    SITE_FILE,
    initial::validation::SiteInputBundleValidator,
    membership::{NodeRemoval, NodeRemovalPlan, OfflineSite, OfflineSiteError, looks_like_node_id},
};

use super::{Notices, boxed, record_value, write_records};
use crate::{
    RootPathArgs,
    management::{ManagementError, ManagerArgs, single_line},
    terminal::{
        ConfirmPrompt, Message, NextStep, PromptError, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// Arguments of `dotc node remove`.
#[derive(Debug, Args)]
pub struct NodeRemoveArgs {
    #[command(flatten)]
    manager: ManagerArgs,

    /// Display name or node id of the worker node to remove.
    #[arg(value_name = "NODE")]
    pub node: String,

    /// Drop the declarations of deployments placed on the node, and the routes only they serve,
    /// without cleanup evidence from the node. Without it, a node with deployments is refused. A
    /// running site accepts it only while the node is not connected.
    #[arg(long)]
    pub forget_deployments: bool,

    /// Remove the node without asking for confirmation.
    #[arg(long)]
    pub yes: bool,
}

/// Removes the node after the operator confirms: through the site manager when it holds the site,
/// else from the stopped site.
pub(crate) async fn node_remove(
    paths: &RootPathArgs,
    arguments: NodeRemoveArgs,
) -> anyhow::Result<()> {
    if !arguments.yes && controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!(
            "removing a node needs a terminal; run in a terminal or pass --yes to remove it without confirmation"
        );
    }
    match open_stopped_site(paths)? {
        Some(site) => remove_from_stopped_site(&site, &arguments),
        None => remove_from_running_site(&arguments).await,
    }
}

/// Opens the stopped site under the exclusive lock; `None` when there is no site here or a
/// running manager holds it, so the removal goes through the manager.
fn open_stopped_site(paths: &RootPathArgs) -> anyhow::Result<Option<OfflineSite>> {
    let site_dir = paths.site_dir()?;
    if !site_dir.join(SITE_FILE).exists() {
        return Ok(None);
    }
    match OfflineSite::open(
        &site_dir,
        &SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore::model::site_declaration_rules(),
        ),
    ) {
        Ok(site) => Ok(Some(site)),
        Err(OfflineSiteError::SiteLocked { .. }) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "cannot remove a node from the site at {}",
            site_dir.display()
        ))),
    }
}

/// Plans, confirms and applies the removal on the stopped site.
fn remove_from_stopped_site(site: &OfflineSite, arguments: &NodeRemoveArgs) -> anyhow::Result<()> {
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

    let review = RemovalReview::stopped(&plan);
    let mut terminal = (!arguments.yes).then(open_terminal);
    if let Some(terminal) = boxed(&mut terminal) {
        confirm(terminal, &review)?;
    }
    site.remove_node(&plan)
        .with_context(|| format!("cannot remove node {:?}", plan.node.display_name))?;
    report_removed(boxed(&mut terminal), &review, false)
}

/// Previews, confirms and applies the removal through the running site manager.
async fn remove_from_running_site(arguments: &NodeRemoveArgs) -> anyhow::Result<()> {
    let manager = arguments
        .manager
        .connect()
        .await
        .map_err(|error| match error {
            ManagementError::Unreachable { .. } => anyhow::Error::new(error).context(
                "removing a node needs the site manager running, or its stopped site on this machine",
            ),
            other => other.into(),
        })?;
    let node_id = resolve_node(&manager, &arguments.node).await?;
    let mut membership = manager.membership();
    let mut request = RemoveMachineRequest {
        node_id,
        forget_deployments: arguments.forget_deployments,
        validate_only: true,
    };
    let preview = membership
        .remove_machine(request.clone())
        .await
        .map_err(|status| call_failed("preview the removal", &status))?
        .into_inner()
        .outcome;
    let review = match preview {
        Some(remove_machine_response::Outcome::WouldRemove(plan)) => RemovalReview::running(&plan),
        Some(remove_machine_response::Outcome::AlreadyRemoved(_)) => {
            let mut terminal = (!arguments.yes).then(open_terminal);
            return report_already_removed(boxed(&mut terminal), &request.node_id);
        }
        Some(remove_machine_response::Outcome::Refused(refusal)) => {
            return Err(refused(&arguments.node, &refusal));
        }
        Some(remove_machine_response::Outcome::Removed(_)) | None => {
            anyhow::bail!("the site manager answered the removal preview unexpectedly")
        }
    };

    let mut terminal = (!arguments.yes).then(open_terminal);
    if let Some(terminal) = boxed(&mut terminal) {
        confirm(terminal, &review)?;
    }
    request.validate_only = false;
    let removed = membership
        .remove_machine(request.clone())
        .await
        .map_err(|status| call_failed("remove the node", &status))?
        .into_inner()
        .outcome;
    match removed {
        Some(remove_machine_response::Outcome::Removed(plan)) => {
            report_removed(boxed(&mut terminal), &RemovalReview::running(&plan), true)
        }
        Some(remove_machine_response::Outcome::AlreadyRemoved(_)) => {
            report_already_removed(boxed(&mut terminal), &request.node_id)
        }
        Some(remove_machine_response::Outcome::Refused(refusal)) => {
            Err(refused(&arguments.node, &refusal))
        }
        Some(remove_machine_response::Outcome::WouldRemove(_)) | None => {
            anyhow::bail!("the site manager answered the removal unexpectedly")
        }
    }
}

/// Finds the node id of the machine named by `selector`, its id or its display name. An unknown
/// id is passed on, so a removal that already finished reports so; an unknown name is an error.
async fn resolve_node(
    manager: &crate::management::Manager,
    selector: &str,
) -> anyhow::Result<String> {
    let machines = manager
        .site()
        .list_machines(ListMachinesRequest {})
        .await
        .map_err(|status| call_failed("list the machines", &status))?
        .into_inner()
        .machines;
    if machines
        .iter()
        .any(|machine| machine.machine_id == selector)
    {
        return Ok(selector.to_owned());
    }
    let named = machines
        .iter()
        .filter(|machine| machine.display_name == selector)
        .collect::<Vec<_>>();
    match named.as_slice() {
        [machine] => Ok(machine.machine_id.clone()),
        [] if looks_like_node_id(selector) => Ok(selector.to_owned()),
        [] => anyhow::bail!(
            "no node {selector:?} in this site; declared nodes: {}",
            machines
                .iter()
                .map(|machine| format!("{:?} ({})", machine.display_name, machine.machine_id))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => anyhow::bail!(
            "several nodes are named {selector:?}; select the node to remove by its id: {}",
            named
                .iter()
                .map(|machine| machine.machine_id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// What the operator reviews before a removal, and what is reported after it.
struct RemovalReview {
    node_id: String,
    name: String,

    /// Facts about the node, such as its host and execution targets.
    facts: Vec<(String, String)>,

    /// Deployments dropped without cleanup: id, kind, and a label with name and state.
    forgotten: Vec<(String, String, String)>,

    /// Routes deleted with them: id and name.
    routes: Vec<(String, String)>,
}

impl RemovalReview {
    fn stopped(plan: &NodeRemovalPlan) -> Self {
        Self {
            node_id: plan.node.node_id.clone(),
            name: plan.node.display_name.clone(),
            facts: vec![
                ("host".to_owned(), plan.node.host.clone()),
                (
                    "targets".to_owned(),
                    target_summary(&plan.execution_targets),
                ),
            ],
            forgotten: plan
                .forgotten_deployments
                .iter()
                .map(|deployment| {
                    (
                        deployment.id.clone(),
                        deployment.kind.to_string(),
                        format!("{} ({})", deployment.name, deployment.state),
                    )
                })
                .collect(),
            routes: plan
                .deleted_routes
                .iter()
                .map(|route| (route.id.clone(), route.name.clone()))
                .collect(),
        }
    }

    fn running(plan: &MachineRemovalPlan) -> Self {
        Self {
            node_id: plan.node_id.clone(),
            name: plan.display_name.clone(),
            facts: vec![(
                "targets".to_owned(),
                match plan.execution_target_ids.len() {
                    0 => "none".to_owned(),
                    count => count.to_string(),
                },
            )],
            forgotten: plan
                .forgotten_deployments
                .iter()
                .map(|deployment| {
                    (
                        deployment.deployment_id.clone(),
                        deployment_kind(deployment).to_owned(),
                        deployment_label(deployment),
                    )
                })
                .collect(),
            routes: plan
                .deleted_routes
                .iter()
                .map(|route| (route.inference_route_id.clone(), route.name.clone()))
                .collect(),
        }
    }
}

/// Shows the node, its execution targets, and every declaration the removal drops, then asks.
fn confirm(terminal: &mut dyn Terminal, review: &RemovalReview) -> anyhow::Result<()> {
    terminal.message(&Message::Heading(format!(
        "Remove {} from the site",
        review.name
    )))?;
    terminal.message(&Message::KeyValues(review.facts.clone()))?;
    if !review.forgotten.is_empty() {
        terminal.message(&Message::Warning(
            "These deployments are dropped without cleaning up the node".to_owned(),
        ))?;
        terminal.message(&Message::KeyValues(
            review
                .forgotten
                .iter()
                .map(|(_, kind, label)| (kind.clone(), label.clone()))
                .collect(),
        ))?;
    }
    if !review.routes.is_empty() {
        terminal.message(&Message::Warning(
            "These routes only served those deployments and are deleted".to_owned(),
        ))?;
        terminal.message(&Message::KeyValues(
            review
                .routes
                .iter()
                .map(|(_, name)| ("route".to_owned(), name.clone()))
                .collect(),
        ))?;
    }
    if terminal.confirm(&ConfirmPrompt::new(
        "Remove this node from the site?",
        false,
    ))? {
        Ok(())
    } else {
        Err(PromptError::Cancelled.into())
    }
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

/// Reports the removal, on the terminal when interactive, else as records. A running site's
/// manager has already told the machine to stop.
fn report_removed(
    terminal: Option<&mut dyn Terminal>,
    review: &RemovalReview,
    running: bool,
) -> anyhow::Result<()> {
    let mut notices = Notices::new(terminal);
    if notices.terminal.is_some() {
        notices.success(&format!("Removed {}", review.name))?;
    } else {
        let mut records = format!(
            "Node removed\nDOTCORE_NODE_ID={}\nDOTCORE_NODE_NAME={}\n",
            record_value(&review.node_id)?,
            record_value(&review.name)?,
        );
        for (id, _, _) in &review.forgotten {
            records.push_str(&format!(
                "DOTCORE_FORGOTTEN_DEPLOYMENT={}\n",
                record_value(id)?
            ));
        }
        for (id, _) in &review.routes {
            records.push_str(&format!("DOTCORE_DELETED_ROUTE={}\n", record_value(id)?));
        }
        write_records(&records)?;
    }
    if running {
        notices.instruct(
            vec![
                NextStep::new("The machine stops Dotcore by itself. To clean it up, run there:")
                    .with_command("dotc node leave"),
            ],
            "The removed machine stops Dotcore by itself; run `dotc node leave` on it to stop its workloads and delete its node folder.",
        )
    } else {
        notices.instruct(
            vec![
                NextStep::new("On that machine, stop its server, then run:")
                    .with_command("dotc node leave"),
            ],
            "Stop the removed node's server, then run `dotc node leave` on that machine to stop its workloads and delete its node folder.",
        )
    }
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

/// The manager's refusal; its message names the deployments, routes or applications that block
/// the removal.
fn refused(selector: &str, refusal: &MachineRemovalRefusal) -> anyhow::Error {
    anyhow::anyhow!("cannot remove node {selector:?}: {}", refusal.message)
}

fn deployment_kind(deployment: &MachineRemovalDeployment) -> &'static str {
    match MachineRemovalDeploymentKind::try_from(deployment.kind) {
        Ok(MachineRemovalDeploymentKind::Application) => "application",
        _ => "model",
    }
}

fn deployment_label(deployment: &MachineRemovalDeployment) -> String {
    if deployment.tearing_down {
        format!("{}, tearing down", deployment.name)
    } else {
        deployment.name.clone()
    }
}

/// Maps a refused management call to an error naming what was attempted.
fn call_failed(action: &str, status: &tonic::Status) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot {action}: the manager returned {:?}: {}",
        status.code(),
        single_line(status.message())
    )
}
