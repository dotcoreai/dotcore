//! The parameters of one application deployment, as rows an operator can read.

use dotcore_proto::dotcore::application::v1::ApplicationDeployment;

use crate::parameters::{ParameterInput, ParameterRow, mask_secret_values, retained};

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

/// Masks the deployment's secret parameter values in place.
pub(super) fn mask_secrets(deployment: &mut ApplicationDeployment) {
    let Some(snapshot) = deployment.snapshot.as_mut() else {
        return;
    };
    let definitions = snapshot
        .graph
        .as_ref()
        .map_or(&[][..], |graph| &graph.parameters);
    mask_secret_values(definitions, &mut snapshot.parameter_values);
}
