//! The parameters of one application deployment, as rows an operator can read.

use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use crate::parameters::{ParameterInput, ParameterRow, retained};

/// The deployment's retained parameters in catalog order. A parameter without a retained value,
/// such as one its switch disabled, is left out.
pub(super) fn rows(deployment: &ApplicationDeployment, reveal_secrets: bool) -> Vec<ParameterRow> {
    let Some(snapshot) = deployment.snapshot.as_ref() else {
        return Vec::new();
    };
    let Some(graph) = snapshot.graph.as_ref() else {
        return Vec::new();
    };

    graph
        .parameters
        .iter()
        .filter_map(|definition| {
            ParameterInput {
                definition,
                value: retained(&snapshot.parameter_values, &definition.parameter_id),
                default: None,
            }
            .row(reveal_secrets)
        })
        .collect()
}
