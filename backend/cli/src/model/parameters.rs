//! The engine parameters of one model deployment, as rows an operator can read.

use dotcore_catalog::typed_parameters::{default_value, is_secret};
use dotcore_proto::dotcore::{
    catalog::v1::{ParameterDefinition, ParameterVisibility, parameter_value},
    model::v1::{
        ModelDeployment, PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot,
        UpstreamModelDesiredConfiguration, model_deployment_snapshot,
        portable_model_launch_argument_token,
    },
};

use crate::parameters::{
    ParameterInput, ParameterRow, SECRET_MASK, mask_secret_values, masked, retained, secret_row,
};

const UPSTREAM_API_KEY_NAME: &str = "API key";

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

/// Masks the deployment's secret values in place: its secret engine parameters, wherever their
/// values appear in the launch arguments, and the upstream API key.
pub(super) fn mask_secrets(deployment: &mut ModelDeployment) {
    if let Some(model_deployment_snapshot::Source::Local(local)) = deployment
        .snapshot
        .as_mut()
        .and_then(|snapshot| snapshot.source.as_mut())
    {
        let definitions = local
            .engine_parameter_schema
            .as_ref()
            .map_or(&[][..], |schema| &schema.parameters);
        let secrets = secret_texts(local, definitions);
        mask_launch_arguments(
            &mut local.desired_launch_plan,
            &mut local.custom_argument_tokens,
            &secrets,
        );
        for values in [
            local.engine_parameter_values.as_mut(),
            local.profile_engine_parameter_defaults.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            mask_secret_values(definitions, &mut values.values);
        }
    }
    if let Some(configuration) = deployment.upstream_desired_configuration.as_mut() {
        configuration.api_key = masked(&configuration.api_key);
    }
}

/// The secret engine parameter values the deployment retains, longest first so a secret that
/// contains another is masked whole.
fn secret_texts(
    local: &ResolvedLocalModelDeploymentSnapshot,
    definitions: &[ParameterDefinition],
) -> Vec<String> {
    let mut secrets = [
        local.engine_parameter_values.as_ref(),
        local.profile_engine_parameter_defaults.as_ref(),
    ]
    .into_iter()
    .flatten()
    .flat_map(|values| &values.values)
    .filter(|value| {
        definitions.iter().any(|definition| {
            definition.parameter_id == value.parameter_id && is_secret(definition)
        })
    })
    .filter_map(|value| match &value.value {
        Some(parameter_value::Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    })
    .collect::<Vec<_>>();
    secrets.sort_by_key(|text| std::cmp::Reverse(text.len()));
    secrets.dedup();
    secrets
}

/// Masks each of `secrets` inside the launch plan's literal arguments and the custom argument
/// tokens, where an engine receives a secret parameter as part of a command line argument.
fn mask_launch_arguments(
    plan: &mut Option<PortableModelLaunchPlan>,
    custom_tokens: &mut [String],
    secrets: &[String],
) {
    if secrets.is_empty() {
        return;
    }
    let mask = |text: &mut String| {
        for secret in secrets {
            *text = text.replace(secret.as_str(), SECRET_MASK);
        }
    };
    for token in plan.iter_mut().flat_map(|plan| &mut plan.arguments) {
        if let Some(portable_model_launch_argument_token::Value::Literal(literal)) =
            token.value.as_mut()
        {
            mask(literal);
        }
    }
    custom_tokens.iter_mut().for_each(mask);
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

    Some(secret_row(UPSTREAM_API_KEY_NAME, key, reveal_secrets))
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterSchema, ParameterValue, ParameterValues, SecretParameterSchema,
            parameter_definition::ParameterType,
        },
        model::v1::{
            ModelDeploymentSnapshot, PortableModelLaunchArgumentToken, PortableModelLaunchPlan,
        },
    };

    use super::*;

    fn literal(text: &str) -> PortableModelLaunchArgumentToken {
        PortableModelLaunchArgumentToken {
            value: Some(portable_model_launch_argument_token::Value::Literal(
                text.to_owned(),
            )),
        }
    }

    fn secret_parameter() -> ParameterValue {
        ParameterValue {
            parameter_id: "api-key".to_owned(),
            value: Some(parameter_value::Value::String("s3cr3t-value".to_owned())),
        }
    }

    #[test]
    fn a_secret_value_is_masked_in_launch_arguments_and_custom_tokens() {
        let local = ResolvedLocalModelDeploymentSnapshot {
            engine_parameter_schema: Some(EngineParameterSchema {
                parameters: vec![ParameterDefinition {
                    parameter_id: "api-key".to_owned(),
                    parameter_type: Some(ParameterType::Secret(SecretParameterSchema {
                        generated_length: 16,
                        minimum_length: 1,
                        maximum_length: None,
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            engine_parameter_values: Some(ParameterValues {
                values: vec![
                    secret_parameter(),
                    ParameterValue {
                        parameter_id: "threads".to_owned(),
                        value: Some(parameter_value::Value::UnsignedInteger(4)),
                    },
                ],
            }),
            desired_launch_plan: Some(PortableModelLaunchPlan {
                arguments: vec![
                    literal("--api-key"),
                    literal("s3cr3t-value"),
                    literal("--header=Bearer s3cr3t-value"),
                    PortableModelLaunchArgumentToken {
                        value: Some(portable_model_launch_argument_token::Value::ServiceHost(
                            "s3cr3t-value".to_owned(),
                        )),
                    },
                ],
                ..Default::default()
            }),
            custom_argument_tokens: vec!["--key=s3cr3t-value".to_owned(), "--verbose".to_owned()],
            ..Default::default()
        };
        let mut deployment = ModelDeployment {
            snapshot: Some(ModelDeploymentSnapshot {
                source: Some(model_deployment_snapshot::Source::Local(local)),
            }),
            ..Default::default()
        };

        mask_secrets(&mut deployment);

        let Some(model_deployment_snapshot::Source::Local(local)) =
            deployment.snapshot.and_then(|snapshot| snapshot.source)
        else {
            panic!("local snapshot");
        };
        let literals = local
            .desired_launch_plan
            .expect("plan")
            .arguments
            .into_iter()
            .map(|token| token.value.expect("token"))
            .collect::<Vec<_>>();
        assert_eq!(
            literals,
            [
                portable_model_launch_argument_token::Value::Literal("--api-key".to_owned()),
                portable_model_launch_argument_token::Value::Literal("********".to_owned()),
                portable_model_launch_argument_token::Value::Literal(
                    "--header=Bearer ********".to_owned()
                ),
                portable_model_launch_argument_token::Value::ServiceHost("s3cr3t-value".to_owned()),
            ]
        );
        assert_eq!(
            local.custom_argument_tokens,
            ["--key=********", "--verbose"]
        );
        let values = local.engine_parameter_values.expect("values").values;
        assert_eq!(
            values[0].value,
            Some(parameter_value::Value::String("********".to_owned()))
        );
        assert_eq!(
            values[1].value,
            Some(parameter_value::Value::UnsignedInteger(4))
        );
    }
}
