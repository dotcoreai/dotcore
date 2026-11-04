//! Shows the model deployments of the running site with their state, node and model, for
//! example one ready Gemma on a GPU node and one paused.
//!
//! On a terminal it prints a table; without one it prints one record block per deployment on
//! stdout, each carrying the id, name, state, node and model.

use clap::Args;
use dotcore_proto::dotcore::model::v1::ModelDeployment;

use super::{
    node_of,
    projection::{model_display_name, presentation_name, state_label},
};
use crate::{
    management::{ManagerArgs, NodeNames, list_deployments, single_line},
    node::write_records,
    terminal::{Message, TerminalCondition, controlling_terminal_condition, open_terminal},
};

/// Arguments for listing model deployments.
#[derive(Debug, Args)]
pub struct ModelListArgs {
    #[command(flatten)]
    manager: ManagerArgs,
}

/// Lists the model deployments on the terminal, else as records.
pub(crate) async fn model_list(arguments: ModelListArgs) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployments = list_deployments::<ModelDeployment>(&manager).await?;
    let nodes = manager.node_names().await;

    if controlling_terminal_condition() == TerminalCondition::Interactive {
        let mut terminal = open_terminal();
        if deployments.is_empty() {
            terminal.message(&Message::Info("No model deployments".to_owned()))?;
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

/// Builds the deployments table, header first.
fn table(deployments: &[ModelDeployment], nodes: &NodeNames) -> Message {
    let rows = deployments
        .iter()
        .map(|deployment| {
            vec![
                single_line(presentation_name(deployment)),
                single_line(&deployment.model_deployment_id),
                state_label(deployment).to_owned(),
                single_line(node_of(deployment, nodes).unwrap_or("-")),
                single_line(model_display_name(deployment)),
            ]
        })
        .collect();

    Message::Table {
        header: ["NAME", "ID", "STATE", "NODE", "MODEL"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        rows,
    }
}

/// Renders one deployment as a record block ending in a newline.
fn record_block(deployment: &ModelDeployment, nodes: &NodeNames) -> String {
    format!(
        "Model deployment\nDOTCORE_MODEL_DEPLOYMENT_ID={}\nDOTCORE_MODEL_DEPLOYMENT_NAME={}\nDOTCORE_MODEL_DEPLOYMENT_STATE={}\nDOTCORE_MODEL_DEPLOYMENT_NODE={}\nDOTCORE_MODEL_DEPLOYMENT_MODEL={}\n",
        single_line(&deployment.model_deployment_id),
        single_line(presentation_name(deployment)),
        state_label(deployment),
        single_line(node_of(deployment, nodes).unwrap_or("")),
        single_line(model_display_name(deployment)),
    )
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::ModelDeployment;

    use super::*;

    #[test]
    fn table_cells_are_collapsed_to_one_line() {
        let deployment = ModelDeployment {
            model_deployment_name: "Chat\n\u{1b}[2Jbot\tone".to_owned(),
            ..Default::default()
        };

        let Message::Table { rows, .. } = table(&[deployment], &NodeNames::default()) else {
            panic!("expected a table");
        };

        assert_eq!(
            rows,
            vec![vec![
                "Chat [2Jbot one".to_owned(),
                String::new(),
                "Unknown".to_owned(),
                "-".to_owned(),
                "Upstream model".to_owned(),
            ]]
        );
    }
}
