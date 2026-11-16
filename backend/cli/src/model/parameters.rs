//! The engine parameters of one model deployment, as rows an operator can read.

use dotcore_catalog::typed_parameters::default_value;
use dotcore_proto::dotcore::{
    catalog::v1::{ParameterDefinition, ParameterVisibility},
    model::v1::{
        ModelDeployment, ResolvedLocalModelDeploymentSnapshot, UpstreamModelDesiredConfiguration,
        model_deployment_snapshot,
    },
};

use crate::parameters::{ParameterInput, ParameterRow, retained, secret_row};

const UPSTREAM_API_KEY_NAME: &str = "API key";
const UPSTREAM_API_KEY_ID: &str = "api-key";

/// The deployment's parameters in engine schema order. Hidden parameters, by the profile's
/// visibility override else the engine's, are left out as in the console; a parameter without a
/// retained value shows its default, and one with neither is left out. An upstream deployment
/// shows its API key, when it has one, as a secret row.
pub(super) fn rows(deployment: &ModelDeployment, reveal_secrets: bool) -> Vec<ParameterRow> {
    match deployment
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.source.as_ref())
    {
        Some(model_deployment_snapshot::Source::Local(local)) => {
            let Some(schema) = local.engine_parameter_schema.as_ref() else {
                return Vec::new();
            };
            let values = local
                .engine_parameter_values
                .as_ref()
                .map_or(&[][..], |values| &values.values);
            let defaults = local
                .profile_engine_parameter_defaults
                .as_ref()
                .map_or(&[][..], |values| &values.values);

            schema
                .parameters
                .iter()
                .filter(|definition| {
                    effective_visibility(local, definition) != ParameterVisibility::Hidden
                })
                .filter_map(|definition| {
                    ParameterInput {
                        definition,
                        value: retained(values, &definition.parameter_id),
                        default: retained(defaults, &definition.parameter_id)
                            .cloned()
                            .or_else(|| default_value(definition)),
                    }
                    .row(reveal_secrets)
                })
                .collect()
        }
        Some(model_deployment_snapshot::Source::Upstream(_)) | None => upstream_api_key(
            deployment.upstream_desired_configuration.as_ref(),
            reveal_secrets,
        )
        .into_iter()
        .collect(),
    }
}

/// The visibility a parameter shows with: the profile's override when it has one, else the
/// engine's own visibility.
fn effective_visibility(
    local: &ResolvedLocalModelDeploymentSnapshot,
    definition: &ParameterDefinition,
) -> ParameterVisibility {
    local
        .profile_engine_parameter_visibility_overrides
        .iter()
        .find(|entry| entry.parameter_id == definition.parameter_id)
        .map_or(definition.visibility(), |entry| entry.visibility())
}

fn upstream_api_key(
    configuration: Option<&UpstreamModelDesiredConfiguration>,
    reveal_secrets: bool,
) -> Option<ParameterRow> {
    let key = configuration
        .map(|configuration| configuration.api_key.as_str())
        .filter(|key| !key.is_empty())?;

    Some(secret_row(
        UPSTREAM_API_KEY_ID,
        UPSTREAM_API_KEY_NAME,
        key,
        reveal_secrets,
    ))
}
