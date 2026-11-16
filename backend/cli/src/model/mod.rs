//! Manages the model deployments of a running site through the site manager's management API,
//! never through the site folder.
//!
//! Deployments are named by id or name and shown with the same state words as the web console,
//! for example "Pausing" for a deployment asked to pause whose workload still runs. On a
//! terminal the commands print tables and messages; without one they print result records on
//! stdout, for example `DOTCORE_MODEL_DEPLOYMENT_STATE=Ready`.

mod list;
mod parameters;
mod projection;
mod show;

use clap::{Args, Subcommand};
use dotcore_proto::dotcore::{
    deployment::v1::ExactPlacement,
    model::v1::{
        DeploymentLogEntry, DeploymentLogResumePosition, DeploymentLogSourceState,
        FollowDeploymentLogsRequest, ListModelDeploymentsRequest, ModelDeployment,
        ModelDeploymentDesiredState, PauseModelRequest, ReadDeploymentLogsRequest,
        RemoveModelDeploymentRequest, ResumeModelRequest, follow_deployment_logs_response,
    },
};
use futures_util::StreamExt;
use tonic::Status;

use crate::management::{
    DeploymentGroup, Lifecycle, LogEntry, LogEvent, LogEvents, LogPage, LogPosition, LogSource,
    LogSourceState, LoggedDeployment, LogsArgs, ManagedDeployment, Manager, ManagerArgs, NodeNames,
    change_lifecycle, deployment_logs, teardown,
};
use list::{ModelListArgs, model_list};
use projection::{desired_state_label, state_label};
use show::{ModelShowArgs, model_show};

/// Commands that manage the model deployments of a running site.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ModelCommand {
    #[command(subcommand)]
    command: Option<ModelSubcommand>,
}

/// Actions supported by the model command group.
#[derive(Debug, Subcommand)]
enum ModelSubcommand {
    /// List the model deployments with their state, node and model; runs against the running
    /// site manager (`--manager`).
    #[command(alias = "ls")]
    List(ModelListArgs),

    /// Show one model deployment by name or id.
    Show(ModelShowArgs),

    /// Ask the manager to pause a model deployment; returns without waiting for the pause.
    Pause(LifecycleArgs),

    /// Ask the manager to resume a paused model deployment; returns without waiting.
    Resume(LifecycleArgs),

    /// Remove a model deployment and its running workload resources after confirmation.
    Teardown(ModelTeardownArgs),

    /// Print the most recent log lines of a model deployment, on any node, and follow new ones
    /// with --follow; lines go to stdout and source states to stderr.
    Logs(LogsArgs),
}

impl ModelCommand {
    /// Runs the chosen action.
    pub(crate) async fn dispatch(self) -> anyhow::Result<()> {
        match self.command {
            Some(ModelSubcommand::List(arguments)) => model_list(arguments).await,
            Some(ModelSubcommand::Show(arguments)) => model_show(arguments).await,
            Some(ModelSubcommand::Pause(arguments)) => {
                change_lifecycle::<ModelDeployment>(
                    &arguments.manager,
                    &arguments.reference,
                    Lifecycle::Pause,
                )
                .await
            }
            Some(ModelSubcommand::Resume(arguments)) => {
                change_lifecycle::<ModelDeployment>(
                    &arguments.manager,
                    &arguments.reference,
                    Lifecycle::Resume,
                )
                .await
            }
            Some(ModelSubcommand::Teardown(arguments)) => {
                teardown::<ModelDeployment>(&arguments.manager, &arguments.reference, arguments.yes)
                    .await
            }
            Some(ModelSubcommand::Logs(arguments)) => {
                deployment_logs::<ModelDeployment>(arguments).await
            }
            None => anyhow::bail!("model command requires an action"),
        }
    }

    /// Reports an argument combination clap cannot reject; this group has none.
    pub(crate) fn argument_contract_error(&self) -> Option<&'static str> {
        None
    }
}

/// Arguments selecting the model deployment to pause or resume.
#[derive(Debug, Args)]
struct LifecycleArgs {
    /// Name or id of the model deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Arguments selecting the model deployment to tear down.
#[derive(Debug, Args)]
struct ModelTeardownArgs {
    /// Name or id of the model deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Tear down without asking for confirmation.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

impl ManagedDeployment for ModelDeployment {
    const GROUP: DeploymentGroup = DeploymentGroup::Model;

    async fn list(manager: &Manager) -> Result<Vec<Self>, Status> {
        manager
            .models()
            .list_model_deployments(ListModelDeploymentsRequest {})
            .await
            .map(|response| response.into_inner().deployments)
    }

    async fn request(
        manager: &Manager,
        id: String,
        lifecycle: Lifecycle,
    ) -> Result<Option<Self>, Status> {
        let mut client = manager.models();
        match lifecycle {
            Lifecycle::Pause => client
                .pause_model(PauseModelRequest {
                    model_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
            Lifecycle::Resume => client
                .resume_model(ResumeModelRequest {
                    model_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
            Lifecycle::Teardown => client
                .remove_model_deployment(RemoveModelDeploymentRequest {
                    model_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
        }
    }

    fn accepts(&self, lifecycle: Lifecycle) -> bool {
        self.desired_state()
            == match lifecycle {
                Lifecycle::Pause => ModelDeploymentDesiredState::Paused,
                Lifecycle::Resume => ModelDeploymentDesiredState::Present,
                Lifecycle::Teardown => ModelDeploymentDesiredState::Teardown,
            }
    }

    fn state_label(&self) -> &'static str {
        state_label(self)
    }

    fn desired_state_label(&self) -> &'static str {
        desired_state_label(self.desired_state())
    }

    fn teardown_consequence(&self) -> String {
        "This removes the deployment and its running workload resources.".to_owned()
    }
}

impl LoggedDeployment for ModelDeployment {
    async fn read_logs(
        manager: &Manager,
        id: String,
        sources: Vec<String>,
        page_size: u32,
    ) -> Result<LogPage, Status> {
        let page = manager
            .models()
            .read_deployment_logs(ReadDeploymentLogsRequest {
                model_deployment_id: id,
                source_names: sources,
                page_size,
                before_timestamp: None,
            })
            .await?
            .into_inner();
        Ok(LogPage {
            sources: page
                .sources
                .into_iter()
                .map(|source| {
                    let state = source.state();
                    log_source(source.source_name, source.source_label, state)
                })
                .collect(),
            entries: page.entries.into_iter().map(log_entry).collect(),
            truncated: page.response_truncated,
            positions: page
                .latest_positions
                .into_iter()
                .map(|position| LogPosition {
                    source_name: position.source_name,
                    timestamp: position.latest_timestamp,
                })
                .collect(),
        })
    }

    async fn follow_logs(
        manager: &Manager,
        id: String,
        sources: Vec<String>,
        positions: Vec<LogPosition>,
    ) -> Result<LogEvents, Status> {
        let events = manager
            .models()
            .follow_deployment_logs(FollowDeploymentLogsRequest {
                model_deployment_id: id,
                source_names: sources,
                resume_positions: positions
                    .into_iter()
                    .map(|position| DeploymentLogResumePosition {
                        source_name: position.source_name,
                        latest_timestamp: position.timestamp,
                    })
                    .collect(),
            })
            .await?
            .into_inner();
        Ok(Box::pin(events.filter_map(|response| async move {
            match response.map(|response| response.event) {
                Ok(Some(follow_deployment_logs_response::Event::SourceEvent(source))) => {
                    let state = source.state();
                    Some(Ok(LogEvent::Source(log_source(
                        source.source_name,
                        source.source_label,
                        state,
                    ))))
                }
                Ok(Some(follow_deployment_logs_response::Event::Entry(entry))) => {
                    Some(Ok(LogEvent::Entry(log_entry(entry))))
                }
                Ok(None) => None,
                Err(status) => Some(Err(status)),
            }
        })))
    }
}

fn log_source(name: String, label: String, state: DeploymentLogSourceState) -> LogSource {
    LogSource {
        name,
        label,
        state: match state {
            DeploymentLogSourceState::Available => LogSourceState::Available,
            DeploymentLogSourceState::Unavailable => LogSourceState::Unavailable,
            DeploymentLogSourceState::Ended => LogSourceState::Ended,
            DeploymentLogSourceState::Unspecified => LogSourceState::Unknown,
        },
    }
}

fn log_entry(entry: DeploymentLogEntry) -> LogEntry {
    LogEntry {
        source_name: entry.source_name,
        source_label: entry.source_label,
        timestamp: entry.timestamp,
        content: entry.content,
        truncated: entry.truncated,
    }
}

/// Where a deployment runs: the node's name (its id when unnamed), or `None` for an upstream
/// deployment, which has no placement.
fn node_of<'a>(deployment: &'a ModelDeployment, nodes: &'a NodeNames) -> Option<&'a str> {
    placement(deployment).map(|placement| nodes.display(&placement.site_node_id))
}

/// The exact placement of a local deployment.
fn placement(deployment: &ModelDeployment) -> Option<&ExactPlacement> {
    deployment
        .intent
        .as_ref()
        .and_then(|intent| intent.placement.as_ref())
}
