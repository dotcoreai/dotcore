//! Shows the model deployments of the running site with their state, node and model, for
//! example one ready Gemma on a GPU node and one paused.
//!
//! It prints a table, on the terminal or as plain text without one. Asked for YAML, it prints the
//! deployments as the manager returns them, with secret values masked.

use clap::Args;
use dotcore_proto::dotcore::model::v1::ModelDeployment;

use super::{
    document, node_of,
    projection::{model_display_name, presentation_name, state_label},
};
use crate::{
    management::{DeploymentListDocument, ManagerArgs, NodeNames, list_deployments, single_line},
    output::{OutputFormat, present, write_yaml},
    terminal::Message,
};

/// Arguments for listing model deployments.
#[derive(Debug, Args)]
pub struct ModelListArgs {
    #[command(flatten)]
    manager: ManagerArgs,
}

/// Lists the model deployments as YAML when asked, else as a table.
pub(crate) async fn model_list(
    arguments: ModelListArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployments = list_deployments::<ModelDeployment>(&manager).await?;
    let nodes = manager.node_names().await;

    if format == OutputFormat::Yaml {
        return write_yaml(&DeploymentListDocument::new(
            deployments
                .into_iter()
                .map(|deployment| document(deployment, &nodes, false))
                .collect(),
        ));
    }

    if deployments.is_empty() {
        return present(&[Message::Info("No model deployments".to_owned())]);
    }
    present(&[table(&deployments, &nodes)])
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
