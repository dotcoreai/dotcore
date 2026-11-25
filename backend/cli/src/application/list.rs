//! Shows the application deployments of the running site with their state, node and
//! application, for example one ready Open WebUI and one paused.
//!
//! It prints a table, on the terminal or as plain text without one. Asked for YAML, it prints the
//! deployments as the manager returns them, with secret values masked.

use clap::Args;
use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use super::{
    document, node_of,
    projection::{application_display_name, observation_stale, presentation_name, state_label},
};
use crate::{
    management::{DeploymentListDocument, ManagerArgs, NodeNames, list_deployments, single_line},
    output::{OutputFormat, present, write_yaml},
    terminal::Message,
};

/// Arguments for listing application deployments.
#[derive(Debug, Args)]
pub struct ApplicationListArgs {
    #[command(flatten)]
    manager: ManagerArgs,
}

/// Lists the application deployments as YAML when asked, else as a table.
pub(crate) async fn application_list(
    arguments: ApplicationListArgs,
    format: OutputFormat,
) -> anyhow::Result<()> {
    let manager = arguments.manager.connect().await?;
    let deployments = list_deployments::<ApplicationDeployment>(&manager).await?;
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
        return present(&[Message::Info("No application deployments".to_owned())]);
    }
    present(&[table(&deployments, &nodes)])
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
