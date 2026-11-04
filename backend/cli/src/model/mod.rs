//! Manages the model deployments of a running site through the site manager's management API,
//! never through the site folder.
//!
//! Deployments are named by id or name and shown with the same state words as the web console,
//! for example "Pausing" for a deployment asked to pause whose workload still runs. On a
//! terminal the commands print tables and messages; without one they print result records on
//! stdout, for example `DOTCORE_MODEL_DEPLOYMENT_STATE=Ready`.

mod list;
mod projection;
mod show;

use clap::{Args, Subcommand};
use dotcore_proto::dotcore::{
    deployment::v1::ExactPlacement,
    model::v1::{
        ListModelDeploymentsRequest, ModelDeployment, ModelDeploymentDesiredState,
        PauseModelRequest, RemoveModelDeploymentRequest, ResumeModelRequest,
    },
};
use tonic::Status;

use crate::management::{
    DeploymentGroup, Lifecycle, ManagedDeployment, Manager, ManagerArgs, NodeNames,
    change_lifecycle, teardown,
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
