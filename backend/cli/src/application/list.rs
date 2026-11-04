//! Shows the application deployments of the running site with their state, node and
//! application, for example one ready Open WebUI and one paused.
//!
//! On a terminal it prints a table; without one it prints one record block per deployment on
//! stdout, each carrying the id, name, state, node, application and whether the state is stale.

use clap::Args;
use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use super::{
    node_of,
    projection::{application_display_name, observation_stale, presentation_name, state_label},
};
use crate::{
    management::{ManagerArgs, NodeNames, list_deployments, single_line},
    node::write_records,
    terminal::{Message, TerminalCondition, controlling_terminal_condition, open_terminal},
};

/// Arguments for listing application deployments.
#[derive(Debug, Args)]
pub struct ApplicationListArgs {
    #[command(flatten)]
    manager: ManagerArgs,
}

/// Lists the application deployments on the terminal, else as records.
pub(crate) async fn application_list(arguments: ApplicationListArgs) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployments = list_deployments::<ApplicationDeployment>(&manager).await?;
    let nodes = manager.node_names().await;

    if controlling_terminal_condition() == TerminalCondition::Interactive {
        let mut terminal = open_terminal();
        if deployments.is_empty() {
            terminal.message(&Message::Info("No application deployments".to_owned()))?;
        } else {
            terminal.message(&table(&deployments, &nodes))?;
        }
        return Ok(());
    }

    let blocks = deployments
        .iter()
        .map(|deployment| record_block(deployment, &nodes))
        .collect::<Vec<_>>();
    write_records(&blocks.join("\n"))
}

/// Builds the deployments table, header first. A state the manager cannot confirm carries a
/// `(stale)` mark.
fn table(deployments: &[ApplicationDeployment], nodes: &NodeNames) -> Message {
    let rows = deployments
        .iter()
        .map(|deployment| {
            let state = match observation_stale(deployment) {
                true => format!("{} (stale)", state_label(deployment)),
                false => state_label(deployment).to_owned(),
            };
            vec![
                single_line(presentation_name(deployment)),
                single_line(&deployment.application_deployment_id),
                state,
                match single_line(node_of(deployment, nodes)) {
                    node if node.is_empty() => "-".to_owned(),
                    node => node,
                },
                single_line(application_display_name(deployment)),
            ]
        })
        .collect();

    Message::Table {
        header: ["NAME", "ID", "STATE", "NODE", "APPLICATION"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        rows,
    }
}

/// Renders one deployment as a record block ending in a newline.
fn record_block(deployment: &ApplicationDeployment, nodes: &NodeNames) -> String {
    format!(
        "Application deployment\nDOTCORE_APPLICATION_DEPLOYMENT_ID={}\nDOTCORE_APPLICATION_DEPLOYMENT_NAME={}\nDOTCORE_APPLICATION_DEPLOYMENT_STATE={}\nDOTCORE_APPLICATION_DEPLOYMENT_STALE={}\nDOTCORE_APPLICATION_DEPLOYMENT_NODE={}\nDOTCORE_APPLICATION_DEPLOYMENT_APPLICATION={}\n",
        single_line(&deployment.application_deployment_id),
        single_line(presentation_name(deployment)),
        state_label(deployment),
        observation_stale(deployment),
        single_line(node_of(deployment, nodes)),
        single_line(application_display_name(deployment)),
    )
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

    use super::*;

    #[test]
    fn table_cells_are_collapsed_to_one_line() {
        let deployment = ApplicationDeployment {
            application_deployment_name: "Wiki\nnext\trow".to_owned(),
            ..Default::default()
        };

        let Message::Table { rows, .. } = table(&[deployment], &NodeNames::default()) else {
            panic!("expected a table");
        };

        assert_eq!(
            rows,
            vec![vec![
                "Wiki next row".to_owned(),
                String::new(),
                "Unknown".to_owned(),
                "-".to_owned(),
                "Application".to_owned(),
            ]]
        );
    }
}
