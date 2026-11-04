//! Manages the application deployments of a running site through the site manager's management
//! API, never through the site folder.
//!
//! Deployments are named by id or name and shown with the same state words as the web console,
//! for example "Pausing" for a deployment asked to pause whose workload still runs. On a
//! terminal the commands print tables and messages; without one they print result records on
//! stdout, for example `DOTCORE_APPLICATION_DEPLOYMENT_STATE=Ready`.

mod list;
mod projection;
mod show;

use clap::{Args, Subcommand};
use dotcore_proto::dotcore::{
    application::v1::{
        ApplicationDeployment, ApplicationDeploymentDesiredState,
        ListApplicationDeploymentsRequest, PauseDeploymentRequest,
        RemoveApplicationDeploymentRequest, ResumeDeploymentRequest,
    },
    deployment::v1::ExactPlacement,
};
use tonic::Status;

use crate::management::{
    DeploymentGroup, Lifecycle, ManagedDeployment, Manager, ManagerArgs, NodeNames,
    change_lifecycle, teardown,
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

    /// Remove an application deployment, its workloads and its data after confirmation.
    Teardown(ApplicationTeardownArgs),
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
