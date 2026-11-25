//! Shows what an operator needs to manage one application deployment, for example its state,
//! node, endpoint, services or failure message.
//!
//! It prints labeled values for what is known, on the terminal or as plain text without one. Asked
//! for YAML, it prints the deployment as the manager returns it, with secret values masked unless revealed.

use clap::Args;
use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use super::{
    document, node_of, parameters,
    projection::{
        application_display_name, desired_state_label, endpoint, observation_stale,
        presentation_name, readiness, readiness_label, service_label, state_label,
        state_overridden,
    },
};
use crate::{
    management::{ManagedDeployment, ManagerArgs, NodeNames, find_deployment, single_line},
    output::{OutputFormat, present, write_yaml},
    parameters::{ParameterRow, parameter_messages},
    terminal::Message,
};

/// Arguments selecting the application deployment to show.
#[derive(Debug, Args)]
pub struct ApplicationShowArgs {
    /// Name or ID of the application deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Print secret parameter values instead of masking them.
    #[arg(long)]
    reveal_secrets: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Shows the deployment as YAML when asked, else as labeled values.
pub(crate) async fn application_show(
    arguments: ApplicationShowArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployment =
        find_deployment::<ApplicationDeployment>(&manager, &arguments.reference).await?;
    let nodes = manager.node_names().await;

    if format == OutputFormat::Yaml {
        return write_yaml(&document(deployment, &nodes, arguments.reveal_secrets));
    }

    let details = Details::of(&deployment, &nodes, arguments.reveal_secrets);

    let mut messages = vec![
        Message::Heading(format!("Application deployment {}", details.name)),
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
    readiness: &'static str,
    stale: bool,
    node: String,
    execution_target: String,
    execution_target_name: String,
    application: String,
    services: Vec<(String, &'static str)>,
    endpoint: String,
    route: String,
    failure: String,
    message: String,
    parameters: Vec<ParameterRow>,
}

impl Details {
    fn of(deployment: &ApplicationDeployment, nodes: &NodeNames, reveal_secrets: bool) -> Self {
        let state = state_label(deployment);
        let observed = deployment.state.as_ref();
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
            .map(|state| single_line(&state.stage))
            .filter(|stage| !stage.eq_ignore_ascii_case(state))
            .unwrap_or_default();
        let services = observed.map_or_else(Vec::new, |state| {
            state
                .services
                .iter()
                .map(|service| {
                    (
                        single_line(&service.service_name),
                        service_label(service.lifecycle_state()),
                    )
                })
                .collect()
        });
        let route = deployment
            .selected_inference_route
            .as_ref()
            .map_or("", |route| {
                if route.route_name.is_empty() {
                    &route.inference_route_id
                } else {
                    &route.route_name
                }
            });
        let readiness = match readiness(deployment) {
            known if readiness_label(known) == "Unknown" => "",
            known => readiness_label(known),
        };

        Self {
            id: single_line(&deployment.application_deployment_id),
            name: single_line(presentation_name(deployment)),
            state,
            desired_state: desired_state_label(deployment.desired_state()),
            readiness,
            stale: observation_stale(deployment),
            node: single_line(node_of(deployment, nodes)),
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
            application: single_line(application_display_name(deployment)),
            services,
            endpoint: single_line(endpoint(deployment)),
            route: single_line(route),
            failure,
            message,
            parameters: parameters::rows(deployment, reveal_secrets),
        }
    }

    /// The labeled values for a terminal, without the ones that are absent. Each service is its
    /// own row and the stale mark shows only when set.
    fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("name".to_owned(), self.name.clone()),
            ("id".to_owned(), self.id.clone()),
            ("state".to_owned(), self.state.to_owned()),
            ("desired".to_owned(), self.desired_state.to_owned()),
            ("readiness".to_owned(), self.readiness.to_owned()),
            (
                "stale".to_owned(),
                if self.stale { "yes" } else { "" }.to_owned(),
            ),
            ("node".to_owned(), self.node.clone()),
            ("target".to_owned(), self.execution_target_name.clone()),
            ("target id".to_owned(), self.execution_target.clone()),
            ("application".to_owned(), self.application.clone()),
        ];
        pairs.extend(
            self.services
                .iter()
                .map(|(name, state)| (format!("service {name}"), (*state).to_owned())),
        );
        pairs.extend([
            ("endpoint".to_owned(), self.endpoint.clone()),
            ("route".to_owned(), self.route.clone()),
            ("failure".to_owned(), self.failure.clone()),
            ("message".to_owned(), self.message.clone()),
        ]);
        pairs.retain(|(_, value)| !value.is_empty());
        pairs
    }
}
