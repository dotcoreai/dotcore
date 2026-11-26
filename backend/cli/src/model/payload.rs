//! The deploy payload of one local model: what to deploy, where, under which name and route, and
//! the parameter and option values to change.
//!
//! The payload is the CLI's own input contract, written by `template` and read by `deploy`. It
//! lists only the values it changes: the server fills everything left out from the profile's
//! defaults and catalog sizing, and validates the result. Values get their parameter's type here,
//! so an unknown or mistyped value fails before anything is sent.

use std::collections::BTreeMap;

use dotcore_proto::dotcore::{
    catalog::v1::{
        ModelConfigurationOption, ModelOptionParameterDefinition, ModelOptionParameterValue,
        ModelOptionValues, ParameterDefinition, ParameterValue, ParameterValues,
        model_option_parameter_definition, model_option_parameter_value, parameter_definition,
        parameter_value,
    },
    model::v1::{
        DirectRouteExposure, HiddenDeploymentExposure, LocalModelDeploymentConfiguration,
        ModelDeploymentExposure, model_deployment_exposure,
    },
};
use serde::{Deserialize, Serialize};

use crate::deploy::{PlainValue, Setting, TypedValue, ValueError, ValueKind};

/// The `--set` prefix that addresses an option parameter rather than an engine parameter.
const OPTION_PREFIX: &str = "option.";

/// One local model deployment as a payload describes it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ModelPayload {
    /// Catalog model reference.
    pub(super) model: String,

    /// Profile ID; the first that fits the target when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) profile: Option<String>,

    /// Node reference; the first node that fits when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) node: Option<String>,

    /// Execution target ID on the node; needed only when the node has several.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) target: Option<String>,

    /// Display name of the deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) name: Option<String>,

    /// Name of the route created with the deployment; none keeps it hidden.
    #[serde(default)]
    pub(super) route: Option<String>,

    /// Engine parameter values by parameter ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) parameters: BTreeMap<String, PlainValue>,

    /// Option parameter values by option ID, then option parameter ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(super) options: BTreeMap<String, BTreeMap<String, PlainValue>>,

    /// Engine arguments appended to the launch as is.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) custom_arguments: Vec<String>,
}

impl ModelPayload {
    /// Parses a payload from YAML (JSON is valid YAML too).
    pub(super) fn parse(text: &str) -> Result<Self, PayloadError> {
        dotcore_proto::yaml::from_str(text).map_err(PayloadError::Parse)
    }

    /// Applies `--set` overrides in order: `option.<option-id>.<parameter-id>` sets an option
    /// parameter, any other key an engine parameter.
    pub(super) fn apply(&mut self, settings: &[Setting]) -> Result<(), PayloadError> {
        for setting in settings {
            match setting.key.strip_prefix(OPTION_PREFIX) {
                Some(rest) => {
                    let (option, parameter) = rest
                        .split_once('.')
                        .filter(|(option, parameter)| !option.is_empty() && !parameter.is_empty())
                        .ok_or_else(|| PayloadError::OptionKey(setting.key.clone()))?;
                    self.options
                        .entry(option.to_owned())
                        .or_default()
                        .insert(parameter.to_owned(), setting.value.clone());
                }
                None => {
                    self.parameters
                        .insert(setting.key.clone(), setting.value.clone());
                }
            }
        }
        Ok(())
    }

    /// The configuration the payload asks for, typed by the profile's schema. Values it leaves out
    /// stay out, for the server to fill.
    pub(super) fn configuration(
        &self,
        schema: &ParameterSchema<'_>,
    ) -> Result<LocalModelDeploymentConfiguration, PayloadError> {
        let values = self
            .parameters
            .iter()
            .map(|(id, value)| engine_value(schema.parameters, id, value))
            .collect::<Result<Vec<_>, _>>()?;
        let option_overrides = self
            .options
            .iter()
            .map(|(id, values)| option_values(schema.options, id, values))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(LocalModelDeploymentConfiguration {
            model_artifact_binding_id: String::new(),
            engine_parameter_values: (!values.is_empty()).then_some(ParameterValues { values }),
            custom_argument_tokens: self.custom_arguments.clone(),
            option_overrides,
        })
    }

    /// Rewrites every value in its parameter's type, so a written payload carries plain numbers
    /// and booleans. Values the schema doesn't type stay as written.
    pub(super) fn normalize(&mut self, schema: &ParameterSchema<'_>) {
        for (id, value) in &mut self.parameters {
            if let Some(definition) = schema
                .parameters
                .iter()
                .find(|definition| definition.parameter_id == *id)
                && let Ok(typed) = value.typed(engine_kind(definition))
            {
                *value = typed.into();
            }
        }
        for (option_id, values) in &mut self.options {
            let Some(option) = schema
                .options
                .iter()
                .find(|option| option.option_id == *option_id)
            else {
                continue;
            };
            for (id, value) in values.iter_mut() {
                if let Some(definition) = option
                    .parameters
                    .iter()
                    .find(|definition| definition.parameter_id == *id)
                    && let Ok(typed) = value.typed(option_kind(definition))
                {
                    *value = typed.into();
                }
            }
        }
    }

    /// The route the deployment is created with: a route of the payload's name, or none.
    pub(super) fn exposure(&self) -> ModelDeploymentExposure {
        ModelDeploymentExposure {
            kind: Some(match &self.route {
                Some(route_name) => model_deployment_exposure::Kind::Direct(DirectRouteExposure {
                    route_name: route_name.clone(),
                }),
                None => model_deployment_exposure::Kind::Hidden(HiddenDeploymentExposure {}),
            }),
        }
    }
}

/// The parameters a payload's values are typed against: the profile's engine parameters and its
/// configuration's options.
pub(super) struct ParameterSchema<'a> {
    /// Engine parameters of the profile's engine configuration.
    pub(super) parameters: &'a [ParameterDefinition],

    /// Options of the profile's engine configuration.
    pub(super) options: &'a [ModelConfigurationOption],
}

/// Why a payload can't become a deployment request.
#[derive(Debug, thiserror::Error)]
pub(super) enum PayloadError {
    /// The text is not a payload.
    #[error("the payload is not valid")]
    Parse(#[source] dotcore_proto::yaml::RestrictedYamlError),

    /// An option override doesn't name an option and one of its parameters.
    #[error("option override {0:?} must be option.<option-id>.<parameter-id>")]
    OptionKey(String),

    /// The profile has no engine parameter with this ID.
    #[error("unknown parameter {id:?}; this profile's parameters are: {known}")]
    UnknownParameter { id: String, known: String },

    /// The configuration has no option with this ID.
    #[error("unknown option {id:?}; this profile's options are: {known}")]
    UnknownOption { id: String, known: String },

    /// The option has no parameter with this ID.
    #[error("unknown parameter {id:?} of option {option:?}; its parameters are: {known}")]
    UnknownOptionParameter {
        option: String,
        id: String,
        known: String,
    },

    /// A value doesn't have its parameter's type.
    #[error("parameter {id:?} {source}")]
    Mistyped {
        id: String,
        #[source]
        source: ValueError,
    },
}

fn engine_value(
    definitions: &[ParameterDefinition],
    id: &str,
    value: &PlainValue,
) -> Result<ParameterValue, PayloadError> {
    let definition = definitions
        .iter()
        .find(|definition| definition.parameter_id == id)
        .ok_or_else(|| PayloadError::UnknownParameter {
            id: id.to_owned(),
            known: known(
                definitions
                    .iter()
                    .map(|definition| &definition.parameter_id),
            ),
        })?;
    let typed = typed(id, value, engine_kind(definition))?;
    Ok(ParameterValue {
        parameter_id: id.to_owned(),
        value: Some(match typed {
            TypedValue::Boolean(value) => parameter_value::Value::Boolean(value),
            TypedValue::UnsignedInteger(value) => parameter_value::Value::UnsignedInteger(value),
            TypedValue::Text(value) => parameter_value::Value::String(value),
        }),
    })
}

fn option_values(
    options: &[ModelConfigurationOption],
    id: &str,
    values: &BTreeMap<String, PlainValue>,
) -> Result<ModelOptionValues, PayloadError> {
    let option = options
        .iter()
        .find(|option| option.option_id == id)
        .ok_or_else(|| PayloadError::UnknownOption {
            id: id.to_owned(),
            known: known(options.iter().map(|option| &option.option_id)),
        })?;
    let values = values
        .iter()
        .map(|(parameter_id, value)| option_value(option, parameter_id, value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ModelOptionValues {
        option_id: id.to_owned(),
        values,
    })
}

fn option_value(
    option: &ModelConfigurationOption,
    id: &str,
    value: &PlainValue,
) -> Result<ModelOptionParameterValue, PayloadError> {
    let definition = option
        .parameters
        .iter()
        .find(|definition| definition.parameter_id == id)
        .ok_or_else(|| PayloadError::UnknownOptionParameter {
            option: option.option_id.clone(),
            id: id.to_owned(),
            known: known(
                option
                    .parameters
                    .iter()
                    .map(|parameter| &parameter.parameter_id),
            ),
        })?;
    let typed = typed(
        &format!("{}.{id}", option.option_id),
        value,
        option_kind(definition),
    )?;
    Ok(ModelOptionParameterValue {
        parameter_id: id.to_owned(),
        value: Some(match typed {
            TypedValue::Boolean(value) => model_option_parameter_value::Value::Boolean(value),
            TypedValue::UnsignedInteger(value) => {
                model_option_parameter_value::Value::UnsignedInteger(value)
            }
            TypedValue::Text(value) => model_option_parameter_value::Value::String(value),
        }),
    })
}

fn typed(id: &str, value: &PlainValue, kind: ValueKind) -> Result<TypedValue, PayloadError> {
    value.typed(kind).map_err(|source| PayloadError::Mistyped {
        id: id.to_owned(),
        source,
    })
}

/// The value type of an engine parameter; a definition without a type takes text, for the
/// server to reject.
pub(super) fn engine_kind(definition: &ParameterDefinition) -> ValueKind {
    match definition.parameter_type {
        Some(parameter_definition::ParameterType::Boolean(_)) => ValueKind::Boolean,
        Some(parameter_definition::ParameterType::UnsignedInteger(_)) => ValueKind::UnsignedInteger,
        Some(
            parameter_definition::ParameterType::StringEnum(_)
            | parameter_definition::ParameterType::Text(_)
            | parameter_definition::ParameterType::Secret(_),
        )
        | None => ValueKind::Text,
    }
}

/// The value type of an option parameter; a definition without a type takes text, for the
/// server to reject.
pub(super) fn option_kind(definition: &ModelOptionParameterDefinition) -> ValueKind {
    match definition.parameter_type {
        Some(model_option_parameter_definition::ParameterType::Boolean(_)) => ValueKind::Boolean,
        Some(model_option_parameter_definition::ParameterType::UnsignedInteger(_)) => {
            ValueKind::UnsignedInteger
        }
        Some(model_option_parameter_definition::ParameterType::StringEnum(_)) | None => {
            ValueKind::Text
        }
    }
}

fn known<'a>(ids: impl Iterator<Item = &'a String>) -> String {
    ids.map(String::as_str).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, StringEnumModelOptionParameterSchema, StringEnumParameterSchema,
        UnsignedIntegerModelOptionParameterSchema, UnsignedIntegerParameterSchema,
    };

    use super::*;

    #[test]
    fn a_payload_becomes_the_sparse_configuration_it_lists() {
        let payload = ModelPayload::parse(
            "model: qwen\nprofile: balanced\nnode: gpu1\nname: Qwen\nroute: qwen\n\
             parameters:\n  context-per-request: 32768\n  flash-attention: true\n  cache-type: q8_0\n\
             options:\n  kv-cache-precision:\n    precision: f16\n\
             customArguments: [--verbose]\n",
        )
        .expect("payload");

        assert_eq!(
            payload.configuration(&schema()).expect("configuration"),
            LocalModelDeploymentConfiguration {
                model_artifact_binding_id: String::new(),
                engine_parameter_values: Some(ParameterValues {
                    values: vec![
                        engine("cache-type", parameter_value::Value::String("q8_0".into())),
                        engine(
                            "context-per-request",
                            parameter_value::Value::UnsignedInteger(32768)
                        ),
                        engine("flash-attention", parameter_value::Value::Boolean(true)),
                    ],
                }),
                custom_argument_tokens: vec!["--verbose".into()],
                option_overrides: vec![ModelOptionValues {
                    option_id: "kv-cache-precision".into(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "precision".into(),
                        value: Some(model_option_parameter_value::Value::String("f16".into())),
                    }],
                }],
            }
        );
        assert_eq!(
            payload.exposure(),
            ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Direct(
                    DirectRouteExposure {
                        route_name: "qwen".into(),
                    }
                )),
            }
        );
    }

    #[test]
    fn a_payload_without_values_or_route_leaves_them_to_the_server_and_stays_hidden() {
        let payload = ModelPayload::parse("model: qwen\nroute: null\n").expect("payload");

        assert_eq!(
            payload.configuration(&schema()).expect("configuration"),
            LocalModelDeploymentConfiguration::default()
        );
        assert_eq!(
            payload.exposure(),
            ModelDeploymentExposure {
                kind: Some(model_deployment_exposure::Kind::Hidden(
                    HiddenDeploymentExposure {}
                )),
            }
        );
    }

    #[test]
    fn settings_override_payload_values_and_reach_option_parameters() {
        let mut payload =
            ModelPayload::parse("model: qwen\nparameters:\n  context-per-request: 8192\n")
                .expect("payload");
        payload
            .apply(&[
                Setting::parse("context-per-request=65536").expect("setting"),
                Setting::parse("option.speculative.token-count=3").expect("setting"),
            ])
            .expect("apply");

        let configuration = payload.configuration(&schema()).expect("configuration");
        assert_eq!(
            configuration.engine_parameter_values,
            Some(ParameterValues {
                values: vec![engine(
                    "context-per-request",
                    parameter_value::Value::UnsignedInteger(65536)
                )],
            })
        );
        assert_eq!(
            configuration.option_overrides,
            vec![ModelOptionValues {
                option_id: "speculative".into(),
                values: vec![ModelOptionParameterValue {
                    parameter_id: "token-count".into(),
                    value: Some(model_option_parameter_value::Value::UnsignedInteger(3)),
                }],
            }]
        );
    }

    #[test]
    fn unknown_or_mistyped_values_fail_naming_the_parameter() {
        let cases = [
            (
                "total=1",
                "unknown parameter \"total\"; this profile's parameters are: \
                 context-per-request, flash-attention, cache-type",
            ),
            (
                "flash-attention=yes",
                "parameter \"flash-attention\" expects true or false",
            ),
            (
                "option.vision.enabled=true",
                "unknown option \"vision\"; this profile's options are: kv-cache-precision, speculative",
            ),
            (
                "option.speculative.depth=1",
                "unknown parameter \"depth\" of option \"speculative\"; its parameters are: token-count",
            ),
            (
                "option.speculative.token-count=many",
                "parameter \"speculative.token-count\" expects a whole number",
            ),
        ];
        for (setting, message) in cases {
            let mut payload = ModelPayload {
                model: "qwen".into(),
                ..ModelPayload::default()
            };
            payload
                .apply(&[Setting::parse(setting).expect("setting")])
                .expect("apply");
            let error = payload.configuration(&schema()).expect_err(setting);
            assert_eq!(error.to_string(), message, "{setting}");
        }
    }

    #[test]
    fn an_option_override_must_name_its_parameter() {
        let mut payload = ModelPayload::default();
        let error = payload
            .apply(&[Setting::parse("option.speculative=3").expect("setting")])
            .expect_err("no parameter");
        assert_eq!(
            error.to_string(),
            "option override \"option.speculative\" must be option.<option-id>.<parameter-id>"
        );
    }

    #[test]
    fn unknown_payload_fields_are_rejected() {
        let error = ModelPayload::parse("model: qwen\nprofle: balanced\n").expect_err("typo");
        assert_eq!(error.to_string(), "the payload is not valid");

        let chain = format!("{:#}", anyhow::Error::new(error));
        assert!(
            chain.contains("unknown field `profle`, expected one of `model`, `profile`"),
            "{chain}"
        );
    }

    fn schema() -> ParameterSchema<'static> {
        use std::sync::LazyLock;

        static PARAMETERS: LazyLock<Vec<ParameterDefinition>> = LazyLock::new(|| {
            vec![
                definition(
                    "context-per-request",
                    parameter_definition::ParameterType::UnsignedInteger(
                        UnsignedIntegerParameterSchema::default(),
                    ),
                ),
                definition(
                    "flash-attention",
                    parameter_definition::ParameterType::Boolean(BooleanParameterSchema::default()),
                ),
                definition(
                    "cache-type",
                    parameter_definition::ParameterType::StringEnum(
                        StringEnumParameterSchema::default(),
                    ),
                ),
            ]
        });
        static OPTIONS: LazyLock<Vec<ModelConfigurationOption>> = LazyLock::new(|| {
            vec![
                option(
                    "kv-cache-precision",
                    "precision",
                    model_option_parameter_definition::ParameterType::StringEnum(
                        StringEnumModelOptionParameterSchema::default(),
                    ),
                ),
                option(
                    "speculative",
                    "token-count",
                    model_option_parameter_definition::ParameterType::UnsignedInteger(
                        UnsignedIntegerModelOptionParameterSchema::default(),
                    ),
                ),
            ]
        });
        ParameterSchema {
            parameters: &PARAMETERS,
            options: &OPTIONS,
        }
    }

    fn definition(id: &str, kind: parameter_definition::ParameterType) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: id.into(),
            parameter_type: Some(kind),
            ..ParameterDefinition::default()
        }
    }

    fn option(
        id: &str,
        parameter: &str,
        kind: model_option_parameter_definition::ParameterType,
    ) -> ModelConfigurationOption {
        ModelConfigurationOption {
            option_id: id.into(),
            parameters: vec![ModelOptionParameterDefinition {
                parameter_id: parameter.into(),
                parameter_type: Some(kind),
                ..ModelOptionParameterDefinition::default()
            }],
            ..ModelConfigurationOption::default()
        }
    }

    fn engine(id: &str, value: parameter_value::Value) -> ParameterValue {
        ParameterValue {
            parameter_id: id.into(),
            value: Some(value),
        }
    }
}
