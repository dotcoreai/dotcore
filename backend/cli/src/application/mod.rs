//! Manages the application deployments of a running site through the site manager's management
//! API, never through the site folder.
//!
//! Deployments are named by id or name and shown with the same state words as the web console,
//! for example "Pausing" for a deployment asked to pause whose workload still runs. On a
//! terminal the commands print tables and messages; without one they print result records on
//! stdout, for example `DOTCORE_APPLICATION_DEPLOYMENT_STATE=Ready`.

mod list;
mod parameters;
mod projection;
mod show;

use clap::{Args, Subcommand};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationDeploymentDesiredState, DeploymentLogEntry,
        DeploymentLogResumePosition, DeploymentLogSourceState, FollowDeploymentLogsRequest,
        ListApplicationDeploymentsRequest, PauseDeploymentRequest, ReadDeploymentLogsRequest,
        RemoveApplicationDeploymentRequest, ResumeDeploymentRequest,
        follow_deployment_logs_response,
    },
    catalog::v1::StoragePlacement,
    deployment::v1::ExactPlacement,
};
use futures_util::StreamExt;
use tonic::Status;

use crate::management::{
    DeploymentGroup, Lifecycle, LogEntry, LogEvent, LogEvents, LogPage, LogPosition, LogSource,
    LogSourceState, LoggedDeployment, LogsArgs, ManagedDeployment, Manager, ManagerArgs, NodeNames,
    change_lifecycle, deployment_logs, single_line, teardown,
};
use list::{ApplicationListArgs, application_list};
use projection::{desired_state_label, state_label};
use show::{ApplicationShowArgs, application_show};

/// Commands that manage the application deployments of a running site.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ApplicationCommand {
    #[command(subcommand)]
    command: Option<ApplicationSubcommand>,
}

/// Actions supported by the application command group.
#[derive(Debug, Subcommand)]
enum ApplicationSubcommand {
    /// List the application deployments with their state, node and application; runs against
    /// the running site manager (`--manager`).
    #[command(alias = "ls")]
    List(ApplicationListArgs),

    /// Show one application deployment by name or id.
    Show(ApplicationShowArgs),

    /// Ask the manager to pause an application deployment; returns without waiting for the pause.
    Pause(LifecycleArgs),

    /// Ask the manager to resume a paused application deployment; returns without waiting.
    Resume(LifecycleArgs),

    /// Remove an application deployment, its workloads and the data placed with it after
    /// confirmation; data shared by the app is kept.
    Teardown(ApplicationTeardownArgs),

    /// Print the most recent log lines of an application deployment, on any node, and follow
    /// new ones with --follow; lines go to stdout and source states to stderr.
    Logs(LogsArgs),
}

impl ApplicationCommand {
    /// Runs the chosen action.
    pub(crate) async fn dispatch(self) -> anyhow::Result<()> {
        match self.command {
            Some(ApplicationSubcommand::List(arguments)) => application_list(arguments).await,
            Some(ApplicationSubcommand::Show(arguments)) => application_show(arguments).await,
            Some(ApplicationSubcommand::Pause(arguments)) => {
                change_lifecycle::<ApplicationDeployment>(
                    &arguments.manager,
                    &arguments.reference,
                    Lifecycle::Pause,
                )
                .await
            }
            Some(ApplicationSubcommand::Resume(arguments)) => {
                change_lifecycle::<ApplicationDeployment>(
                    &arguments.manager,
                    &arguments.reference,
                    Lifecycle::Resume,
                )
                .await
            }
            Some(ApplicationSubcommand::Teardown(arguments)) => {
                teardown::<ApplicationDeployment>(
                    &arguments.manager,
                    &arguments.reference,
                    arguments.yes,
                )
                .await
            }
            Some(ApplicationSubcommand::Logs(arguments)) => {
                deployment_logs::<ApplicationDeployment>(arguments).await
            }
            None => anyhow::bail!("application command requires an action"),
        }
    }

    /// Reports an argument combination clap cannot reject; this group has none.
    pub(crate) fn argument_contract_error(&self) -> Option<&'static str> {
        None
    }
}

/// Arguments selecting the application deployment to pause or resume.
#[derive(Debug, Args)]
struct LifecycleArgs {
    /// Name or id of the application deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Arguments selecting the application deployment to tear down.
#[derive(Debug, Args)]
struct ApplicationTeardownArgs {
    /// Name or id of the application deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Tear down without asking for confirmation.
    #[arg(long)]
    yes: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

impl ManagedDeployment for ApplicationDeployment {
    const GROUP: DeploymentGroup = DeploymentGroup::Application;

    async fn list(manager: &Manager) -> Result<Vec<Self>, Status> {
        manager
            .applications()
            .list_application_deployments(ListApplicationDeploymentsRequest {})
            .await
            .map(|response| response.into_inner().deployments)
    }

    async fn request(
        manager: &Manager,
        id: String,
        lifecycle: Lifecycle,
    ) -> Result<Option<Self>, Status> {
        let mut client = manager.applications();
        match lifecycle {
            Lifecycle::Pause => client
                .pause_deployment(PauseDeploymentRequest {
                    application_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
            Lifecycle::Resume => client
                .resume_deployment(ResumeDeploymentRequest {
                    application_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
            Lifecycle::Teardown => client
                .remove_application_deployment(RemoveApplicationDeploymentRequest {
                    application_deployment_id: id,
                })
                .await
                .map(|response| response.into_inner().deployment),
        }
    }

    fn accepts(&self, lifecycle: Lifecycle) -> bool {
        self.desired_state()
            == match lifecycle {
                Lifecycle::Pause => ApplicationDeploymentDesiredState::Paused,
                Lifecycle::Resume => ApplicationDeploymentDesiredState::Present,
                Lifecycle::Teardown => ApplicationDeploymentDesiredState::Teardown,
            }
    }

    fn state_label(&self) -> &'static str {
        state_label(self)
    }

    fn desired_state_label(&self) -> &'static str {
        desired_state_label(self.desired_state())
    }

    fn teardown_consequence(&self) -> String {
        teardown_consequence(self)
    }
}

/// What tearing a deployment down does to its data: storage placed with the deployment is
/// deleted, storage shared by the app is kept, each listed by binding name.
fn teardown_consequence(deployment: &ApplicationDeployment) -> String {
    let bindings = |placement: StoragePlacement| {
        deployment
            .storage_attachments
            .iter()
            .filter(|attachment| attachment.placement == placement as i32)
            .map(|attachment| single_line(&attachment.binding_name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let deleted = bindings(StoragePlacement::Deployment);
    let kept = bindings(StoragePlacement::Application);

    let mut text = "This stops and removes its workloads".to_owned();
    if !deleted.is_empty() {
        text.push_str(&format!(" and deletes this deployment's data ({deleted})"));
    }
    if kept.is_empty() {
        text.push_str(". The catalog application is kept.");
    } else {
        text.push_str(&format!(
            ". It keeps the data shared by the app ({kept}) and the catalog application."
        ));
    }
    text
}

impl LoggedDeployment for ApplicationDeployment {
    async fn read_logs(
        manager: &Manager,
        id: String,
        sources: Vec<String>,
        page_size: u32,
    ) -> Result<LogPage, Status> {
        let page = manager
            .applications()
            .read_deployment_logs(ReadDeploymentLogsRequest {
                application_deployment_id: id,
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
            .applications()
            .follow_deployment_logs(FollowDeploymentLogsRequest {
                application_deployment_id: id,
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

/// Where a deployment runs: the node's name, or its id when unnamed; empty without a placement.
fn node_of<'a>(deployment: &'a ApplicationDeployment, nodes: &'a NodeNames) -> &'a str {
    deployment
        .placement
        .as_ref()
        .map_or("", |placement| nodes.display(&placement.site_node_id))
}

/// The exact placement of a deployment.
fn placement(deployment: &ApplicationDeployment) -> Option<&ExactPlacement> {
    deployment.placement.as_ref()
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::application::v1::ApplicationStorageAttachment;

    use super::*;

    #[test]
    fn teardown_consequence_names_the_deleted_and_the_kept_data() {
        let with_storage = |attachments: &[(&str, StoragePlacement)]| ApplicationDeployment {
            storage_attachments: attachments
                .iter()
                .map(|(binding, placement)| ApplicationStorageAttachment {
                    service_name: "web".to_owned(),
                    binding_name: (*binding).to_owned(),
                    storage_name: format!("web-{binding}"),
                    placement: *placement as i32,
                })
                .collect(),
            ..Default::default()
        };

        assert_eq!(
            teardown_consequence(&with_storage(&[])),
            "This stops and removes its workloads. The catalog application is kept."
        );
        assert_eq!(
            teardown_consequence(&with_storage(&[
                ("data", StoragePlacement::Deployment),
                ("models", StoragePlacement::Deployment),
            ])),
            "This stops and removes its workloads and deletes this deployment's data (data, models). The catalog application is kept."
        );
        assert_eq!(
            teardown_consequence(&with_storage(&[("models", StoragePlacement::Application)])),
            "This stops and removes its workloads. It keeps the data shared by the app (models) and the catalog application."
        );
    }
}
