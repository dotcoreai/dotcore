//! Shows what an operator needs to manage one model deployment, for example its state, node,
//! model, download progress or failure message.
//!
//! It prints labeled values for what is known, on the terminal or as plain text without one. Asked
//! for YAML, it prints the deployment as the manager returns it, with secret values masked unless revealed.

use clap::Args;
use dotcore_proto::dotcore::model::v1::{ModelDeployment, model_deployment_snapshot};

use super::{
    document, node_of, parameters,
    projection::{
        desired_state_label, model_display_name, presentation_name, state_label, state_overridden,
    },
};
use crate::{
    management::{ManagedDeployment, ManagerArgs, NodeNames, find_deployment, single_line},
    output::{OutputFormat, present, write_yaml},
    parameters::{ParameterRow, parameter_messages},
    terminal::Message,
};

/// Arguments selecting the model deployment to show.
#[derive(Debug, Args)]
pub struct ModelShowArgs {
    /// Name or ID of the model deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Print secret parameter values instead of masking them.
    #[arg(long)]
    reveal_secrets: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Shows the deployment as YAML when asked, else as labeled values.
pub(crate) async fn model_show(
    arguments: ModelShowArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployment = find_deployment::<ModelDeployment>(&manager, &arguments.reference).await?;
    let nodes = manager.node_names().await;

    if format == OutputFormat::Yaml {
        return write_yaml(&document(deployment, &nodes, arguments.reveal_secrets));
    }

    let details = Details::of(&deployment, &nodes, arguments.reveal_secrets);

    let mut messages = vec![
        Message::Heading(format!("Model deployment {}", details.name)),
        Message::KeyValues(details.pairs()),
    ];
    messages.extend(parameter_messages(&details.parameters));
    present(&messages)
}

/// The shown facts of one deployment; absent facts are empty.
struct Details {
    id: String,
    name: String,
    state: &'static str,
    desired_state: &'static str,
    node: String,
    execution_target: String,
    execution_target_name: String,
    model: String,
    profile: String,
    engine: String,
    progress: String,
    failure: String,
    message: String,
    parameters: Vec<ParameterRow>,
}

impl Details {
    fn of(deployment: &ModelDeployment, nodes: &NodeNames, reveal_secrets: bool) -> Self {
        let state = state_label(deployment);
        let observed = deployment.state.as_ref();
        let (profile, engine) = match deployment
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.source.as_ref())
        {
            Some(model_deployment_snapshot::Source::Local(local)) => (
                local.profile_id.as_str(),
                local
                    .engine_artifact
                    .as_ref()
                    .map_or("", |artifact| artifact.engine_release_id.as_str()),
            ),
            _ => ("", ""),
        };
        let failure = observed
            .and_then(|state| state.failure.as_ref())
            .map_or_else(String::new, |failure| {
                let text = if failure.message.is_empty() {
                    &failure.code
                } else {
                    &failure.message
                };
                single_line(text)
            });
        let message = observed
            .filter(|_| !state_overridden(deployment))
            .map(|state| single_line(&state.status_message))
            .filter(|message| !message.eq_ignore_ascii_case(state))
            .unwrap_or_default();
        let progress = observed
            .and_then(|state| state.progress.as_ref())
            .and_then(|progress| progress.percent)
            .map_or_else(String::new, |percent| format!("{percent}%"));

        Self {
            id: single_line(&deployment.model_deployment_id),
            name: single_line(presentation_name(deployment)),
            state,
            desired_state: desired_state_label(deployment.desired_state()),
            node: single_line(node_of(deployment, nodes).unwrap_or("")),
            execution_target: deployment
                .placement()
                .map_or_else(String::new, |placement| {
                    single_line(&placement.execution_target_id)
                }),
            execution_target_name: deployment
                .placement()
                .map_or_else(String::new, |placement| {
                    single_line(
                        nodes.target_display(
                            &placement.site_node_id,
                            &placement.execution_target_id,
                        ),
                    )
                }),
            model: single_line(model_display_name(deployment)),
            profile: single_line(profile),
            engine: single_line(engine),
            progress,
            failure,
            message,
            parameters: parameters::rows(deployment, reveal_secrets),
        }
    }

    /// The labeled values for a terminal, without the ones that are absent.
    fn pairs(&self) -> Vec<(String, String)> {
        [
            ("name", self.name.as_str()),
            ("id", &self.id),
            ("state", self.state),
            ("desired", self.desired_state),
            ("node", &self.node),
            ("target", &self.execution_target_name),
            ("target id", &self.execution_target),
            ("model", &self.model),
            ("profile", &self.profile),
            ("engine", &self.engine),
            ("progress", &self.progress),
            ("failure", &self.failure),
            ("message", &self.message),
        ]
        .into_iter()
        .filter(|(_, value)| !value.is_empty())
        .map(|(label, value)| (label.to_owned(), value.to_owned()))
        .collect()
    }
}
