//! The parameters an operator can set on one local model deployment, with what each accepts and
//! the value a deployment starts from.
//!
//! The view presents what the server resolved: the parameter schema, visibility, and starting
//! values of a deployment preview, with the context values sizing starts from on the chosen target.
//! It decides no value itself. Hidden parameters are left out, and secret values never appear.

use dotcore_catalog::typed_parameters::{canonical_text, is_secret};
use dotcore_proto::dotcore::{
    catalog::v1::{
        Model, ModelOptionParameterDefinition, ParameterDefinition, ParameterVisibility,
        model_option_parameter_definition, model_option_parameter_value, parameter_definition,
    },
    model::v1::{
        ModelProfileCompatibility, ResolvedLocalModelDeploymentSnapshot, StartingTotalContextSource,
    },
};

use std::collections::BTreeMap;

use super::payload::{engine_kind, option_kind};
use crate::deploy::{PlainValue, ValueKind};

/// The engine parameter one request may use as context, in tokens.
pub(super) const CONTEXT_PER_REQUEST: &str = "context-per-request";

/// The engine parameter holding the context all requests share, in tokens.
pub(super) const TOTAL_CONTEXT: &str = "total-context";

/// The engine parameter counting the requests generated at once.
pub(super) const PARALLEL_REQUESTS: &str = "parallel-requests";

/// The settable parameters of one deployment, engine parameters first in schema order, then
/// option parameters in configuration order.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ParameterView {
    /// The shown parameters.
    pub(super) parameters: Vec<ViewParameter>,
}

/// One parameter as the operator sees it.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct ViewParameter {
    /// The key a payload and `--set` use: the engine parameter ID, or `<option>.<parameter>` for
    /// an option parameter.
    pub(super) key: String,

    /// Human-readable label.
    pub(super) name: String,

    /// One line telling a newcomer what it does; for an option parameter, the option's.
    pub(super) summary: String,

    /// What the value accepts, in operator words.
    pub(super) accepts: String,

    /// The type its value takes.
    pub(super) kind: ValueKind,

    /// Whether the parameter shows in the main view or only among advanced ones.
    pub(super) advanced: bool,

    /// The value a deployment starts from; none for a secret or a value nothing sets.
    pub(super) starting: Option<String>,

    /// Whether the value follows the context per request and parallel requests until set, as the
    /// total context does.
    pub(super) follows: bool,

    /// Whether this is an option parameter rather than an engine parameter.
    pub(super) option: bool,
}

impl ParameterView {
    /// The view of one previewed deployment of `model` on the target `compatibility` evaluates.
    pub(super) fn new(
        model: &Model,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
        compatibility: Option<&ModelProfileCompatibility>,
    ) -> Self {
        let mut parameters = engine_parameters(snapshot, compatibility);
        parameters.extend(option_parameters(model, snapshot));
        Self { parameters }
    }

    /// The parallel requests a deployment runs with: the value `set` gives, else the starting
    /// value. None when neither is a number.
    pub(super) fn parallel_requests(&self, set: &BTreeMap<String, PlainValue>) -> Option<u64> {
        let text = match set.get(PARALLEL_REQUESTS) {
            Some(PlainValue::Number(count)) => return Some(*count),
            Some(PlainValue::Text(text)) => Some(text.as_str()),
            Some(PlainValue::Boolean(_)) => None,
            None => self
                .parameters
                .iter()
                .find(|parameter| parameter.key == PARALLEL_REQUESTS)
                .and_then(|parameter| parameter.starting.as_deref()),
        };
        text.and_then(|text| text.parse().ok())
    }
}

fn engine_parameters(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    compatibility: Option<&ModelProfileCompatibility>,
) -> Vec<ViewParameter> {
    let Some(schema) = snapshot.engine_parameter_schema.as_ref() else {
        return Vec::new();
    };
    let resolved = snapshot
        .engine_parameter_values
        .as_ref()
        .map_or(&[][..], |values| &values.values);
    let total_follows = compatibility.is_none_or(|entry| {
        !matches!(
            entry.starting_total_context_source(),
            StartingTotalContextSource::LargestFit | StartingTotalContextSource::ScenarioTotal
        )
    });

    schema
        .parameters
        .iter()
        .filter_map(|definition| {
            let visibility = snapshot
                .profile_engine_parameter_visibility_overrides
                .iter()
                .find(|entry| entry.parameter_id == definition.parameter_id)
                .map_or(definition.visibility(), |entry| entry.visibility());
            if visibility == ParameterVisibility::Hidden {
                return None;
            }

            let id = definition.parameter_id.as_str();
            let sized = compatibility.and_then(|entry| match id {
                CONTEXT_PER_REQUEST => entry.starting_context_tokens_per_request,
                TOTAL_CONTEXT => entry.starting_total_context_tokens,
                _ => None,
            });
            let starting = if is_secret(definition) {
                None
            } else {
                sized.map(|tokens| tokens.to_string()).or_else(|| {
                    resolved
                        .iter()
                        .find(|value| value.parameter_id == id)
                        .and_then(|value| value.value.as_ref())
                        .map(canonical_text)
                })
            };
            Some(ViewParameter {
                key: id.to_owned(),
                name: definition.display_name.clone(),
                summary: definition.summary.clone(),
                accepts: engine_accepts(definition),
                kind: engine_kind(definition),
                advanced: visibility == ParameterVisibility::Advanced,
                starting,
                follows: id == TOTAL_CONTEXT && total_follows,
                option: false,
            })
        })
        .collect()
}

fn option_parameters(
    model: &Model,
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Vec<ViewParameter> {
    let Some(configuration) = model
        .configurations
        .iter()
        .find(|configuration| configuration.configuration_id == snapshot.configuration_id)
    else {
        return Vec::new();
    };

    configuration
        .options
        .iter()
        .filter_map(|option| {
            let declared = model
                .options
                .iter()
                .find(|declared| declared.option_id == option.option_id)?;
            (declared.visibility() != ParameterVisibility::Hidden).then_some((option, declared))
        })
        .flat_map(|(option, declared)| {
            let resolved = snapshot
                .resolved_options
                .iter()
                .filter_map(|resolved| resolved.values.as_ref())
                .find(|values| values.option_id == option.option_id);
            option
                .parameters
                .iter()
                .map(move |definition| ViewParameter {
                    key: format!("{}.{}", option.option_id, definition.parameter_id),
                    name: format!("{} {}", declared.display_name, definition.display_name),
                    summary: declared.summary.clone(),
                    accepts: option_accepts(definition),
                    kind: option_kind(definition),
                    advanced: declared.visibility() == ParameterVisibility::Advanced,
                    starting: resolved
                        .and_then(|values| {
                            values
                                .values
                                .iter()
                                .find(|value| value.parameter_id == definition.parameter_id)
                        })
                        .and_then(|value| value.value.as_ref())
                        .map(option_text),
                    follows: false,
                    option: true,
                })
        })
        .collect()
}

fn engine_accepts(definition: &ParameterDefinition) -> String {
    use parameter_definition::ParameterType;

    match &definition.parameter_type {
        Some(ParameterType::Boolean(_)) => "true or false".to_owned(),
        Some(ParameterType::UnsignedInteger(range)) => {
            let bounds = match range.maximum {
                Some(maximum) => format!("{}-{maximum}", range.minimum),
                None => format!("{} or more", range.minimum),
            };
            with_unit(bounds, &range.unit)
        }
        Some(ParameterType::StringEnum(schema)) => one_of(&schema.values),
        Some(ParameterType::Text(_)) => "text".to_owned(),
        Some(ParameterType::Secret(_)) => "secret, generated when left out".to_owned(),
        None => "any value".to_owned(),
    }
}

fn option_accepts(definition: &ModelOptionParameterDefinition) -> String {
    use model_option_parameter_definition::ParameterType;

    match &definition.parameter_type {
        Some(ParameterType::Boolean(_)) => "true or false".to_owned(),
        Some(ParameterType::UnsignedInteger(range)) => {
            with_unit(format!("{}-{}", range.minimum, range.maximum), &range.unit)
        }
        Some(ParameterType::StringEnum(schema)) => one_of(&schema.values),
        None => "any value".to_owned(),
    }
}

fn with_unit(bounds: String, unit: &str) -> String {
    if unit.is_empty() {
        bounds
    } else {
        format!("{bounds} {unit}")
    }
}

fn one_of(values: &[String]) -> String {
    format!("one of {}", values.join(" | "))
}

fn option_text(value: &model_option_parameter_value::Value) -> String {
    match value {
        model_option_parameter_value::Value::Boolean(value) => value.to_string(),
        model_option_parameter_value::Value::UnsignedInteger(value) => value.to_string(),
        model_option_parameter_value::Value::String(value) => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::{
        catalog::v1::{
            BooleanParameterSchema, EngineParameterSchema, ModelConfigurationOption,
            ModelEngineConfiguration, ModelOption, ModelOptionParameterValue, ModelOptionValues,
            ParameterValue, ParameterValues, ParameterVisibilityOverride, SecretParameterSchema,
            StringEnumModelOptionParameterSchema, UnsignedIntegerParameterSchema, parameter_value,
        },
        model::v1::ResolvedModelOption,
    };

    use super::*;

    #[test]
    fn the_view_shows_visible_parameters_with_their_starting_values() {
        let view = ParameterView::new(&model(), &snapshot(), Some(&sized(None)));

        assert_eq!(
            view,
            ParameterView {
                parameters: vec![
                    parameter(
                        "context-per-request",
                        "Context per request",
                        "4096-262144 tokens",
                        ValueKind::UnsignedInteger,
                        false,
                        Some("32768")
                    ),
                    ViewParameter {
                        follows: true,
                        ..parameter(
                            "total-context",
                            "Total context",
                            "4096 or more tokens",
                            ValueKind::UnsignedInteger,
                            false,
                            Some("131072")
                        )
                    },
                    parameter(
                        "flash-attention",
                        "Flash attention",
                        "true or false",
                        ValueKind::Boolean,
                        true,
                        Some("true")
                    ),
                    parameter(
                        "api-key",
                        "API key",
                        "secret, generated when left out",
                        ValueKind::Text,
                        true,
                        None
                    ),
                    ViewParameter {
                        option: true,
                        summary: "Precision of the context cache.".into(),
                        ..parameter(
                            "kv-cache-precision.precision",
                            "KV cache precision Precision",
                            "one of f16 | q8_0",
                            ValueKind::Text,
                            true,
                            Some("q8_0")
                        )
                    },
                ],
            }
        );
    }

    #[test]
    fn a_total_context_capped_at_the_largest_fit_no_longer_follows() {
        let view = ParameterView::new(&model(), &snapshot(), Some(&sized(Some((16384, 65536)))));

        let context: Vec<_> = view.parameters[..2]
            .iter()
            .map(|parameter| {
                (
                    parameter.key.as_str(),
                    parameter.starting.as_deref(),
                    parameter.follows,
                )
            })
            .collect();
        assert_eq!(
            context,
            [
                ("context-per-request", Some("16384"), false),
                ("total-context", Some("65536"), false),
            ]
        );
    }

    #[test]
    fn parallel_requests_come_from_the_set_value_else_the_starting_one() {
        let view = ParameterView {
            parameters: vec![parameter(
                "parallel-requests",
                "Parallel requests",
                "1-64",
                ValueKind::UnsignedInteger,
                false,
                Some("4"),
            )],
        };
        let set = |value| BTreeMap::from([("parallel-requests".to_owned(), value)]);

        assert_eq!(view.parallel_requests(&BTreeMap::new()), Some(4));
        assert_eq!(view.parallel_requests(&set(PlainValue::Number(2))), Some(2));
        assert_eq!(
            view.parallel_requests(&set(PlainValue::Text("3".into()))),
            Some(3)
        );
        assert_eq!(
            view.parallel_requests(&set(PlainValue::Text("many".into()))),
            None
        );
        assert_eq!(
            ParameterView {
                parameters: Vec::new()
            }
            .parallel_requests(&BTreeMap::new()),
            None
        );
    }

    fn parameter(
        key: &str,
        name: &str,
        accepts: &str,
        kind: ValueKind,
        advanced: bool,
        starting: Option<&str>,
    ) -> ViewParameter {
        ViewParameter {
            key: key.into(),
            name: name.into(),
            summary: format!("What {name} does."),
            accepts: accepts.into(),
            kind,
            advanced,
            starting: starting.map(Into::into),
            follows: false,
            option: false,
        }
    }

    fn model() -> Model {
        Model {
            configurations: vec![ModelEngineConfiguration {
                configuration_id: "gguf".into(),
                options: vec![ModelConfigurationOption {
                    option_id: "kv-cache-precision".into(),
                    parameters: vec![ModelOptionParameterDefinition {
                        parameter_id: "precision".into(),
                        display_name: "Precision".into(),
                        parameter_type: Some(
                            model_option_parameter_definition::ParameterType::StringEnum(
                                StringEnumModelOptionParameterSchema {
                                    values: vec!["f16".into(), "q8_0".into()],
                                },
                            ),
                        ),
                    }],
                    ..ModelConfigurationOption::default()
                }],
                ..ModelEngineConfiguration::default()
            }],
            options: vec![
                ModelOption {
                    option_id: "kv-cache-precision".into(),
                    display_name: "KV cache precision".into(),
                    summary: "Precision of the context cache.".into(),
                    visibility: ParameterVisibility::Advanced.into(),
                    ..ModelOption::default()
                },
                ModelOption {
                    option_id: "vision".into(),
                    display_name: "Vision".into(),
                    visibility: ParameterVisibility::Basic.into(),
                    ..ModelOption::default()
                },
            ],
            ..Model::default()
        }
    }

    fn snapshot() -> ResolvedLocalModelDeploymentSnapshot {
        let tokens = |minimum, maximum| {
            parameter_definition::ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
                minimum,
                maximum,
                unit: "tokens".into(),
                ..UnsignedIntegerParameterSchema::default()
            })
        };
        ResolvedLocalModelDeploymentSnapshot {
            configuration_id: "gguf".into(),
            engine_parameter_schema: Some(EngineParameterSchema {
                parameters: vec![
                    definition(
                        "context-per-request",
                        "Context per request",
                        ParameterVisibility::Basic,
                        tokens(4096, Some(262_144)),
                    ),
                    definition(
                        "total-context",
                        "Total context",
                        ParameterVisibility::Basic,
                        tokens(4096, None),
                    ),
                    definition(
                        "flash-attention",
                        "Flash attention",
                        ParameterVisibility::Basic,
                        parameter_definition::ParameterType::Boolean(
                            BooleanParameterSchema::default(),
                        ),
                    ),
                    definition(
                        "batch-size",
                        "Batch size",
                        ParameterVisibility::Advanced,
                        tokens(1, None),
                    ),
                    definition(
                        "api-key",
                        "API key",
                        ParameterVisibility::Advanced,
                        parameter_definition::ParameterType::Secret(
                            SecretParameterSchema::default(),
                        ),
                    ),
                ],
                ..EngineParameterSchema::default()
            }),
            profile_engine_parameter_visibility_overrides: vec![
                ParameterVisibilityOverride {
                    parameter_id: "flash-attention".into(),
                    visibility: ParameterVisibility::Advanced.into(),
                },
                ParameterVisibilityOverride {
                    parameter_id: "batch-size".into(),
                    visibility: ParameterVisibility::Hidden.into(),
                },
            ],
            engine_parameter_values: Some(ParameterValues {
                values: vec![
                    value(
                        "context-per-request",
                        parameter_value::Value::UnsignedInteger(32768),
                    ),
                    value(
                        "total-context",
                        parameter_value::Value::UnsignedInteger(131_072),
                    ),
                    value("flash-attention", parameter_value::Value::Boolean(true)),
                    value(
                        "api-key",
                        parameter_value::Value::String("generated".into()),
                    ),
                ],
            }),
            resolved_options: vec![ResolvedModelOption {
                values: Some(ModelOptionValues {
                    option_id: "kv-cache-precision".into(),
                    values: vec![ModelOptionParameterValue {
                        parameter_id: "precision".into(),
                        value: Some(model_option_parameter_value::Value::String("q8_0".into())),
                    }],
                }),
            }],
            ..ResolvedLocalModelDeploymentSnapshot::default()
        }
    }

    /// A compatibility entry starting from 131072 tokens that follow, or from the given lowered
    /// context per request and a total capped at the largest fit.
    fn sized(capped: Option<(u64, u64)>) -> ModelProfileCompatibility {
        match capped {
            None => ModelProfileCompatibility {
                starting_total_context_tokens: Some(131_072),
                starting_total_context_source: StartingTotalContextSource::PerRequestTimesParallel
                    .into(),
                ..ModelProfileCompatibility::default()
            },
            Some((per_request, total)) => ModelProfileCompatibility {
                starting_context_tokens_per_request: Some(per_request),
                starting_total_context_tokens: Some(total),
                starting_total_context_source: StartingTotalContextSource::LargestFit.into(),
                ..ModelProfileCompatibility::default()
            },
        }
    }

    fn definition(
        id: &str,
        name: &str,
        visibility: ParameterVisibility,
        kind: parameter_definition::ParameterType,
    ) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: id.into(),
            display_name: name.into(),
            summary: format!("What {name} does."),
            visibility: visibility.into(),
            parameter_type: Some(kind),
            ..ParameterDefinition::default()
        }
    }

    fn value(id: &str, value: parameter_value::Value) -> ParameterValue {
        ParameterValue {
            parameter_id: id.into(),
            value: Some(value),
        }
    }
}
