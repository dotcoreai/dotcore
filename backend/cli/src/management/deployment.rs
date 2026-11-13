//! The steps that model and application deployment management share: reading the deployments,
//! finding the one an operator names, asking the manager to pause, resume or tear it down, and
//! reporting what it accepted, for example "Pausing Gemma".
//!
//! The manager decides whether a change is allowed and carries it out later, so a request returns
//! once it is accepted and never waits for the workloads. A teardown asks on the terminal first
//! and cancels by default; without a terminal it needs explicit consent.

use tonic::Status;

use super::{
    Addressable, DeploymentGroup, ManagementError, Manager, ManagerArgs, call_failed, resolve,
    single_line,
};
use crate::{
    node::write_records,
    terminal::{
        ConfirmPrompt, Message, NextStep, PromptError, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
};

/// A kind of deployment the management commands handle alike, implemented by the model and the
/// application deployment messages. The steps are generic over it rather than taking a trait
/// object because each kind's calls return its own message type.
pub(crate) trait ManagedDeployment: Addressable + Clone {
    /// The command group the kind belongs to.
    const GROUP: DeploymentGroup;

    /// Reads every deployment of this kind the manager knows.
    async fn list(manager: &Manager) -> Result<Vec<Self>, Status>;

    /// Asks the manager for a lifecycle change of the deployment with this id and returns the
    /// deployment as the manager now holds it.
    async fn request(
        manager: &Manager,
        id: String,
        lifecycle: Lifecycle,
    ) -> Result<Option<Self>, Status>;

    /// Whether the deployment's desired state is the one the lifecycle change asks for.
    fn accepts(&self, lifecycle: Lifecycle) -> bool;

    /// The console's word for the deployment's effective state, for example `Pausing`.
    fn state_label(&self) -> &'static str;

    /// The word for the desired state the manager drives the deployment toward.
    fn desired_state_label(&self) -> &'static str;

    /// What tearing the deployment down removes and keeps, asked after its name in the
    /// confirmation.
    fn teardown_consequence(&self) -> String;
}

/// The lifecycle change an operator asks for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Lifecycle {
    Pause,

    Resume,

    Teardown,
}

impl Lifecycle {
    /// The command word, for example `pause`.
    fn verb(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Teardown => "tear down",
        }
    }

    /// The success line start, for example `Pausing`.
    fn progressive(self) -> &'static str {
        match self {
            Self::Pause => "Pausing",
            Self::Resume => "Resuming",
            Self::Teardown => "Tearing down",
        }
    }
}

/// Reads every deployment of the kind the manager knows.
pub(crate) async fn list_deployments<T: ManagedDeployment>(
    manager: &Manager,
) -> Result<Vec<T>, ManagementError> {
    T::list(manager)
        .await
        .map_err(call_failed(T::GROUP, format!("list {}s", T::GROUP.noun())))
}

/// Finds the one deployment an operator reference names, failing when it names none or several.
pub(crate) async fn find_deployment<T: ManagedDeployment>(
    manager: &Manager,
    reference: &str,
) -> anyhow::Result<T> {
    let deployments = list_deployments::<T>(manager).await?;
    Ok(resolve(T::GROUP, reference, &deployments)?.clone())
}

/// Pauses or resumes the referenced deployment and reports the accepted state.
pub(crate) async fn change_lifecycle<T: ManagedDeployment>(
    manager: &ManagerArgs,
    reference: &str,
    lifecycle: Lifecycle,
) -> anyhow::Result<()> {
    let manager = manager.connect().await?;
    let deployment = find_deployment::<T>(&manager, reference).await?;
    let accepted = request(&manager, &deployment, lifecycle).await?;
    report(lifecycle, &accepted)
}

/// Tears down the referenced deployment after the operator confirms, or right away with `yes`,
/// and reports the accepted state.
pub(crate) async fn teardown<T: ManagedDeployment>(
    manager: &ManagerArgs,
    reference: &str,
    yes: bool,
) -> anyhow::Result<()> {
    require_teardown_approval(T::GROUP, yes)?;

    let manager = manager.connect().await?;
    let deployment = find_deployment::<T>(&manager, reference).await?;
    if !yes {
        confirm_teardown(format!(
            "Tear down {}? {}",
            single_line(deployment.name()),
            deployment.teardown_consequence()
        ))?;
    }

    let accepted = request(&manager, &deployment, Lifecycle::Teardown).await?;
    report(Lifecycle::Teardown, &accepted)
}

/// Sends the request for the deployment and returns the deployment as the manager now holds it,
/// failing when the manager refuses or answers without the asked desired state.
async fn request<T: ManagedDeployment>(
    manager: &Manager,
    deployment: &T,
    lifecycle: Lifecycle,
) -> Result<T, ManagementError> {
    let action = format!("{} {}", lifecycle.verb(), deployment.name());
    let response = T::request(manager, deployment.id().to_owned(), lifecycle)
        .await
        .map_err(call_failed(T::GROUP, action.clone()))?;

    match response {
        Some(accepted) if accepted.accepts(lifecycle) => Ok(accepted),
        other => Err(ManagementError::NotAccepted {
            group: T::GROUP,
            action,
            desired: other
                .as_ref()
                .map_or("Unspecified", ManagedDeployment::desired_state_label)
                .to_owned(),
        }),
    }
}

/// Reports the accepted request: a success line and the next step on the terminal, else one
/// record block on stdout and a notice on stderr.
fn report<T: ManagedDeployment>(lifecycle: Lifecycle, accepted: &T) -> anyhow::Result<()> {
    let name = single_line(accepted.name());
    let list = T::GROUP.list_command();

    if controlling_terminal_condition() == TerminalCondition::Interactive {
        let mut terminal = open_terminal();
        terminal.message(&Message::Success(format!(
            "{} {name}",
            lifecycle.progressive()
        )))?;
        terminal.message(&Message::Next(vec![
            NextStep::new("Follow its progress:").with_command(list),
        ]))?;
        return Ok(());
    }

    let prefix = T::GROUP.record_prefix();
    write_records(&format!(
        "{}\n{prefix}_ID={}\n{prefix}_NAME={name}\n{prefix}_STATE={}\n{prefix}_DESIRED_STATE={}\n",
        T::GROUP.record_title(),
        single_line(accepted.id()),
        accepted.state_label(),
        accepted.desired_state_label(),
    ))?;
    eprintln!("Notice: Run `{list}` to follow its progress.");
    Ok(())
}

/// Fails when a teardown has neither a terminal to ask on nor `--yes`, before any call to the
/// manager.
fn require_teardown_approval(group: DeploymentGroup, yes: bool) -> anyhow::Result<()> {
    if !yes && controlling_terminal_condition() != TerminalCondition::Interactive {
        anyhow::bail!(
            "tearing down {} needs a terminal; run in a terminal or pass --yes to tear it down without confirmation",
            group.indefinite_noun()
        );
    }
    Ok(())
}

/// Asks the operator on the terminal to confirm a teardown, cancelling by default.
fn confirm_teardown(question: String) -> anyhow::Result<()> {
    if open_terminal().confirm(&ConfirmPrompt::new(question, false))? {
        Ok(())
    } else {
        Err(PromptError::Cancelled.into())
    }
}
