//! Shows what an operator needs to manage one application deployment, for example its state,
//! node, endpoint, services or failure message.
//!
//! On a terminal it prints labeled values for what is known; without one it prints one record
//! block on stdout that always carries every key, empty when the value is absent.

use clap::Args;
use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use super::{
    node_of, parameters, placement,
    projection::{
        application_display_name, desired_state_label, endpoint, observation_stale,
        presentation_name, readiness, readiness_label, service_label, state_label,
        state_overridden,
    },
};
use crate::{
    management::{ManagerArgs, NodeNames, find_deployment, single_line},
    node::write_records,
    terminal::{Message, TerminalCondition, controlling_terminal_condition, open_terminal},
};

/// Arguments selecting the application deployment to show.
#[derive(Debug, Args)]
pub struct ApplicationShowArgs {
    /// Name or id of the application deployment.
    #[arg(value_name = "DEPLOYMENT")]
    reference: String,

    /// Print secret parameter values instead of masking them.
    #[arg(long)]
    reveal_secrets: bool,

    #[command(flatten)]
    manager: ManagerArgs,
}

/// Shows the deployment on the terminal, else as a record block.
pub(crate) async fn application_show(arguments: ApplicationShowArgs) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployment =
        find_deployment::<ApplicationDeployment>(&manager, &arguments.reference).await?;
    let nodes = manager.node_names().await;
    let details = Details::of(&deployment, &nodes, arguments.reveal_secrets);

    if controlling_terminal_condition() == TerminalCondition::Interactive {
        let mut terminal = open_terminal();
        terminal.message(&Message::Heading(format!(
            "Application deployment {}",
            details.name
        )))?;
        terminal.message(&Message::KeyValues(details.pairs()))?;
        if !details.parameters.is_empty() {
            terminal.message(&Message::Heading("Parameters".to_owned()))?;
            terminal.message(&Message::KeyValues(
                details
                    .parameters
                    .iter()
                    .map(|row| (row.name.clone(), row.value.clone()))
                    .collect(),
            ))?;
        }
        return Ok(());
    }

    write_records(&details.record_block())
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
    parameters: Vec<parameters::ParameterRow>,
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
            execution_target: placement(deployment).map_or_else(String::new, |placement| {
                single_line(&placement.execution_target_id)
            }),
            execution_target_name: placement(deployment).map_or_else(String::new, |placement| {
                single_line(
                    nodes.target_display(&placement.site_node_id, &placement.execution_target_id),
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

    fn record_block(&self) -> String {
        let services = self
            .services
            .iter()
            .map(|(name, state)| format!("{name}: {state}"))
            .collect::<Vec<_>>()
            .join(", ");
        let mut block = format!(
            "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID={}\nDOTCORE_APPLICATION_DEPLOYMENT_NAME={}\nDOTCORE_APPLICATION_DEPLOYMENT_STATE={}\nDOTCORE_APPLICATION_DEPLOYMENT_DESIRED_STATE={}\nDOTCORE_APPLICATION_DEPLOYMENT_READINESS={}\nDOTCORE_APPLICATION_DEPLOYMENT_STALE={}\nDOTCORE_APPLICATION_DEPLOYMENT_NODE={}\nDOTCORE_APPLICATION_DEPLOYMENT_EXECUTION_TARGET={}\nDOTCORE_APPLICATION_DEPLOYMENT_APPLICATION={}\nDOTCORE_APPLICATION_DEPLOYMENT_SERVICES={services}\nDOTCORE_APPLICATION_DEPLOYMENT_ENDPOINT={}\nDOTCORE_APPLICATION_DEPLOYMENT_ROUTE={}\nDOTCORE_APPLICATION_DEPLOYMENT_FAILURE={}\nDOTCORE_APPLICATION_DEPLOYMENT_MESSAGE={}\n",
            self.id,
            self.name,
            self.state,
            self.desired_state,
            self.readiness,
            self.stale,
            self.node,
            self.execution_target,
            self.application,
            self.endpoint,
            self.route,
            self.failure,
            self.message,
        );
        for row in &self.parameters {
            block.push_str(&format!(
                "DOTCORE_APPLICATION_DEPLOYMENT_PARAMETER_{}={}\n",
                parameters::record_key(&row.id),
                row.value,
            ));
        }
        block
    }
}
