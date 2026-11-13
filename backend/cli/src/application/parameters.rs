//! The parameters of one application deployment, as rows an operator can read.

use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use crate::parameters::{ParameterInput, ParameterRow, definition_default, retained};

/// The deployment's parameters in catalog order. A parameter without a retained value shows its
/// default, and one with neither is left out.
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
                default: definition_default(definition),
            }
            .row(reveal_secrets)
        })
        .collect()
}
