//! The engine parameters every engine release shares: total context, context per request and
//! parallel requests.
//!
//! The catalog owns their ids and their fixed shape and visibility, so controllers, adapters and
//! the console name and place them the same way on every engine, while each release sets its own maxima, default values
//! and descriptions. It also fills the context per request a deployment leaves out, from the
//! maximum context and the total context.

use dotcore_proto::dotcore::catalog::v1::{
    EngineParameterSchema, Model, ModelEngineConfiguration, ParameterDefinition, ParameterValue,
    ParameterValues, ParameterVisibility, parameter_definition::ParameterType,
    parameter_value::Value,
};

/// Id of the total context: the KV pool an engine holds at start, shared by active requests.
pub const TOTAL_CONTEXT_PARAMETER_ID: &str = "total-context";

/// Id of the context per request: the largest context one request may use.
pub const CONTEXT_PER_REQUEST_PARAMETER_ID: &str = "context-per-request";

/// Id of the parallel requests: requests an engine generates at once, while others queue.
pub const PARALLEL_REQUESTS_PARAMETER_ID: &str = "parallel-requests";

/// Smallest total context and context per request a release declares and a deployment or sizing
/// scenario may use, in tokens.
// One ninfer KV page, the smallest context ninfer accepts, so every engine accepts it.
pub const MINIMUM_CONTEXT_TOKENS: u64 = 1024;

/// Whether the parameter is one of the shared parameters, which adapters read by id rather than
/// through a configuration's parameter references.
pub fn is_shared_parameter(parameter_id: &str) -> bool {
    SHARED_PARAMETERS
        .iter()
        .any(|shared| shared.parameter_id == parameter_id)
}

/// Largest context one request may use on a configuration, in tokens: the model's maximum
/// context, or the configuration's lower maximum.
pub fn maximum_context_tokens(model: &Model, configuration: &ModelEngineConfiguration) -> u32 {
    configuration
        .maximum_context_tokens
        .map_or(model.maximum_context_tokens, |maximum| {
            maximum.min(model.maximum_context_tokens)
        })
}

/// Context per request for values that leave it out: the maximum context, or the total context
/// when it is smaller. Returns none when the schema doesn't declare a context per request, the
/// values already supply one, or they hold no total context to bound it.
pub fn filled_context_per_request(
    schema: &EngineParameterSchema,
    values: &ParameterValues,
    maximum_context_tokens: u32,
) -> Option<ParameterValue> {
    let declared = schema
        .parameters
        .iter()
        .any(|definition| definition.parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID);
    let supplied = values
        .values
        .iter()
        .any(|value| value.parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID);
    if !declared || supplied {
        return None;
    }

    let total_context = values.values.iter().find_map(|value| {
        match (
            value.parameter_id == TOTAL_CONTEXT_PARAMETER_ID,
            &value.value,
        ) {
            (true, Some(Value::UnsignedInteger(total_context))) => Some(*total_context),
            _ => None,
        }
    })?;
    Some(ParameterValue {
        parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
        value: Some(Value::UnsignedInteger(
            total_context.min(u64::from(maximum_context_tokens)),
        )),
    })
}

/// Whether a profile may set a default for the parameter. Profiles carry no context: the engine
/// default and the deployment set the total context, and the context per request is filled.
pub(crate) fn profile_may_default(parameter_id: &str) -> bool {
    SHARED_PARAMETERS
        .iter()
        .find(|shared| shared.parameter_id == parameter_id)
        .is_none_or(|shared| shared.profile_default)
}

/// Whether the parameter is fixed in the main view: the context parameters are, and may only be
/// hidden there; an engine or a profile may show any other parameter there too.
pub(crate) fn in_main_view(parameter_id: &str) -> bool {
    SHARED_PARAMETERS.iter().any(|shared| {
        shared.parameter_id == parameter_id && shared.visibility == ParameterVisibility::Basic
    })
}

/// Returns why a profile may not give the parameter this visibility, or none when it may. The
/// context parameters belong to the main view: a profile may hide them but not move them to the
/// advanced view. A profile may give any other parameter any visibility.
pub(crate) fn profile_visibility_problem(
    parameter_id: &str,
    visibility: ParameterVisibility,
) -> Option<&'static str> {
    let main_view = in_main_view(parameter_id);
    match visibility {
        ParameterVisibility::Unspecified => Some("requires a basic, advanced or hidden visibility"),
        ParameterVisibility::Advanced if main_view => Some("may be hidden but not advanced"),
        ParameterVisibility::Basic
        | ParameterVisibility::Advanced
        | ParameterVisibility::Hidden => None,
    }
}

/// Checks that an engine parameter schema declares every shared parameter with its fixed shape,
/// returning one error per problem. Engine payloads are left to the engine's own validation.
pub(crate) fn validate_shared_parameters(schema: &EngineParameterSchema) -> Vec<anyhow::Error> {
    SHARED_PARAMETERS
        .iter()
        .filter_map(|shared| {
            let Some(definition) = schema
                .parameters
                .iter()
                .find(|definition| definition.parameter_id == shared.parameter_id)
            else {
                return Some(anyhow::anyhow!(
                    "engine parameter schema {:?} must declare the shared parameter {:?}",
                    schema.schema_id,
                    shared.parameter_id
                ));
            };
            shared.shape_problem(definition).map(|problem| {
                anyhow::anyhow!(
                    "shared engine parameter {:?} of schema {:?} {problem}",
                    shared.parameter_id,
                    schema.schema_id
                )
            })
        })
        .collect()
}

/// The fixed shape of every shared parameter, in display order.
const SHARED_PARAMETERS: [SharedParameter; 3] = [
    SharedParameter {
        parameter_id: TOTAL_CONTEXT_PARAMETER_ID,
        display_name: "Total context",
        unit: "tokens",
        minimum: MINIMUM_CONTEXT_TOKENS,
        visibility: ParameterVisibility::Basic,
        engine_default: true,
        profile_default: false,
    },
    SharedParameter {
        parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID,
        display_name: "Context per request",
        unit: "tokens",
        minimum: MINIMUM_CONTEXT_TOKENS,
        visibility: ParameterVisibility::Basic,
        engine_default: false,
        profile_default: false,
    },
    SharedParameter {
        parameter_id: PARALLEL_REQUESTS_PARAMETER_ID,
        display_name: "Parallel requests",
        unit: "requests",
        minimum: 1,
        visibility: ParameterVisibility::Advanced,
        engine_default: true,
        profile_default: true,
    },
];

/// One shared parameter's fixed shape. Every shared parameter is an unsigned integer the console
/// shows: the contexts in its main view, the parallel requests in its advanced view.
struct SharedParameter {
    parameter_id: &'static str,
    display_name: &'static str,
    unit: &'static str,
    minimum: u64,
    visibility: ParameterVisibility,

    /// Whether every release must declare an engine default; otherwise none may.
    engine_default: bool,

    /// Whether a profile may set a default.
    profile_default: bool,
}

impl SharedParameter {
    /// Returns how a declared definition departs from the fixed shape, or none when it matches.
    fn shape_problem(&self, definition: &ParameterDefinition) -> Option<String> {
        let Some(ParameterType::UnsignedInteger(range)) = definition.parameter_type.as_ref() else {
            return Some("must be an unsigned integer".to_owned());
        };
        if definition.display_name != self.display_name {
            return Some(format!("must be named {:?}", self.display_name));
        }
        if definition.visibility() != self.visibility {
            return Some(format!("must be {}", visibility_name(self.visibility)));
        }
        if range.unit != self.unit {
            return Some(format!("must be in {:?}", self.unit));
        }
        if range.minimum != self.minimum {
            return Some(format!("must have a minimum of {}", self.minimum));
        }
        match (self.engine_default, range.default_value.is_some()) {
            (true, false) => Some("requires an engine default".to_owned()),
            (false, true) => Some("takes no engine default".to_owned()),
            _ => None,
        }
    }
}

/// Returns the visibility's lowercase name, for messages.
fn visibility_name(visibility: ParameterVisibility) -> &'static str {
    match visibility {
        ParameterVisibility::Unspecified => "unspecified",
        ParameterVisibility::Basic => "basic",
        ParameterVisibility::Advanced => "advanced",
        ParameterVisibility::Hidden => "hidden",
    }
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::UnsignedIntegerParameterSchema;

    use super::*;

    #[test]
    fn a_schema_with_every_shared_parameter_in_its_shape_is_accepted() {
        assert!(validate_shared_parameters(&shared_schema()).is_empty());
    }

    #[test]
    fn a_schema_missing_a_shared_parameter_is_rejected() {
        let mut schema = shared_schema();
        schema
            .parameters
            .retain(|definition| definition.parameter_id != CONTEXT_PER_REQUEST_PARAMETER_ID);

        assert_eq!(
            messages(&schema),
            [
                "engine parameter schema \"fixture-shared\" must declare the shared parameter \"context-per-request\""
            ]
        );
    }

    #[test]
    fn a_shared_parameter_in_the_wrong_shape_is_rejected() {
        let cases: [(&str, ShapeChange, &str); 9] = [
            (
                TOTAL_CONTEXT_PARAMETER_ID,
                |definition| {
                    definition.parameter_type = Some(ParameterType::Boolean(Default::default()));
                },
                "must be an unsigned integer",
            ),
            (
                TOTAL_CONTEXT_PARAMETER_ID,
                |definition| definition.display_name = "Pool size".to_owned(),
                "must be named \"Total context\"",
            ),
            (
                PARALLEL_REQUESTS_PARAMETER_ID,
                |definition| definition.set_visibility(ParameterVisibility::Hidden),
                "must be advanced",
            ),
            (
                TOTAL_CONTEXT_PARAMETER_ID,
                |definition| definition.set_visibility(ParameterVisibility::Advanced),
                "must be basic",
            ),
            (
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                |definition| definition.set_visibility(ParameterVisibility::Hidden),
                "must be basic",
            ),
            (
                PARALLEL_REQUESTS_PARAMETER_ID,
                |definition| range(definition).unit = "slots".to_owned(),
                "must be in \"requests\"",
            ),
            (
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                |definition| range(definition).minimum = 1,
                "must have a minimum of 1024",
            ),
            (
                TOTAL_CONTEXT_PARAMETER_ID,
                |definition| range(definition).default_value = None,
                "requires an engine default",
            ),
            (
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                |definition| range(definition).default_value = Some(4096),
                "takes no engine default",
            ),
        ];
        for (parameter_id, change, problem) in cases {
            let mut schema = shared_schema();
            change(
                schema
                    .parameters
                    .iter_mut()
                    .find(|definition| definition.parameter_id == parameter_id)
                    .expect("shared parameter"),
            );

            assert_eq!(
                messages(&schema),
                [format!(
                    "shared engine parameter {parameter_id:?} of schema \"fixture-shared\" {problem}"
                )],
                "{problem}"
            );
        }
    }

    #[test]
    fn a_schema_declaring_no_shared_parameter_is_rejected_for_each() {
        let schema = EngineParameterSchema {
            schema_id: "fixture-other".to_owned(),
            parameters: vec![definition("memory-bytes", "Memory", "bytes", 1, None)],
        };

        assert_eq!(
            messages(&schema),
            [
                "engine parameter schema \"fixture-other\" must declare the shared parameter \"total-context\"",
                "engine parameter schema \"fixture-other\" must declare the shared parameter \"context-per-request\"",
                "engine parameter schema \"fixture-other\" must declare the shared parameter \"parallel-requests\"",
            ]
        );
    }

    #[test]
    fn only_the_shared_parameters_are_shared() {
        assert!(is_shared_parameter(TOTAL_CONTEXT_PARAMETER_ID));
        assert!(is_shared_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID));
        assert!(is_shared_parameter(PARALLEL_REQUESTS_PARAMETER_ID));
        assert!(!is_shared_parameter("kv-cache-memory-bytes"));
    }

    #[test]
    fn profiles_may_default_parallel_requests_and_engine_parameters_but_no_context() {
        assert!(!profile_may_default(TOTAL_CONTEXT_PARAMETER_ID));
        assert!(!profile_may_default(CONTEXT_PER_REQUEST_PARAMETER_ID));
        assert!(profile_may_default(PARALLEL_REQUESTS_PARAMETER_ID));
        assert!(profile_may_default("prompt-cache-memory-mib"));
    }

    #[test]
    fn profiles_may_hide_any_parameter_and_keep_the_contexts_basic() {
        use ParameterVisibility::{Advanced, Basic, Hidden, Unspecified};

        for parameter_id in [TOTAL_CONTEXT_PARAMETER_ID, CONTEXT_PER_REQUEST_PARAMETER_ID] {
            assert_eq!(profile_visibility_problem(parameter_id, Basic), None);
            assert_eq!(profile_visibility_problem(parameter_id, Hidden), None);
            assert_eq!(
                profile_visibility_problem(parameter_id, Advanced),
                Some("may be hidden but not advanced")
            );
        }
        for parameter_id in [PARALLEL_REQUESTS_PARAMETER_ID, "prompt-cache-memory-mib"] {
            assert_eq!(profile_visibility_problem(parameter_id, Basic), None);
            assert_eq!(profile_visibility_problem(parameter_id, Advanced), None);
            assert_eq!(profile_visibility_problem(parameter_id, Hidden), None);
        }
        assert_eq!(
            profile_visibility_problem(TOTAL_CONTEXT_PARAMETER_ID, Unspecified),
            Some("requires a basic, advanced or hidden visibility")
        );
    }

    #[test]
    fn the_omitted_context_per_request_is_the_smaller_of_the_maximum_and_the_total_context() {
        let schema = shared_schema();
        let filled = |total_context: u64, maximum: u32| {
            filled_context_per_request(&schema, &total(total_context), maximum)
        };

        assert_eq!(filled(32_768, 262_144), Some(context_per_request(32_768)));
        assert_eq!(
            filled(1_048_576, 262_144),
            Some(context_per_request(262_144))
        );
        assert_eq!(filled(262_144, 262_144), Some(context_per_request(262_144)));
    }

    #[test]
    fn the_context_per_request_is_not_filled_when_supplied_undeclared_or_unbounded() {
        let schema = shared_schema();
        let mut supplied = total(32_768);
        supplied.values.push(context_per_request(8192));
        let undeclared = EngineParameterSchema {
            schema_id: "fixture-other".to_owned(),
            parameters: vec![definition("memory-bytes", "Memory", "bytes", 1, None)],
        };

        assert_eq!(
            filled_context_per_request(&schema, &supplied, 262_144),
            None
        );
        assert_eq!(
            filled_context_per_request(&undeclared, &total(32_768), 262_144),
            None
        );
        assert_eq!(
            filled_context_per_request(&schema, &ParameterValues::default(), 262_144),
            None
        );
    }

    #[test]
    fn a_configuration_maximum_lowers_the_model_maximum_context() {
        let model = Model {
            maximum_context_tokens: 262_144,
            ..Default::default()
        };
        let configuration = |maximum_context_tokens| ModelEngineConfiguration {
            maximum_context_tokens,
            ..Default::default()
        };

        assert_eq!(
            maximum_context_tokens(&model, &configuration(None)),
            262_144
        );
        assert_eq!(
            maximum_context_tokens(&model, &configuration(Some(131_072))),
            131_072
        );
        assert_eq!(
            maximum_context_tokens(&model, &configuration(Some(524_288))),
            262_144
        );
    }

    /// Edits one shared parameter definition away from its fixed shape.
    type ShapeChange = fn(&mut ParameterDefinition);

    fn messages(schema: &EngineParameterSchema) -> Vec<String> {
        validate_shared_parameters(schema)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    fn shared_schema() -> EngineParameterSchema {
        EngineParameterSchema {
            schema_id: "fixture-shared".to_owned(),
            parameters: vec![
                definition(
                    TOTAL_CONTEXT_PARAMETER_ID,
                    "Total context",
                    "tokens",
                    1024,
                    Some(32_768),
                ),
                definition(
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    "Context per request",
                    "tokens",
                    1024,
                    None,
                ),
                definition(
                    PARALLEL_REQUESTS_PARAMETER_ID,
                    "Parallel requests",
                    "requests",
                    1,
                    Some(4),
                ),
            ],
        }
    }

    fn definition(
        parameter_id: &str,
        display_name: &str,
        unit: &str,
        minimum: u64,
        default_value: Option<u64>,
    ) -> ParameterDefinition {
        ParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            display_name: display_name.to_owned(),
            parameter_type: Some(ParameterType::UnsignedInteger(
                UnsignedIntegerParameterSchema {
                    minimum,
                    maximum: 16_777_216,
                    unit: unit.to_owned(),
                    default_value,
                },
            )),
            visibility: SHARED_PARAMETERS
                .iter()
                .find(|shared| shared.parameter_id == parameter_id)
                .map_or(ParameterVisibility::Advanced, |shared| shared.visibility)
                as i32,
            description: String::new(),
        }
    }

    fn range(definition: &mut ParameterDefinition) -> &mut UnsignedIntegerParameterSchema {
        match definition.parameter_type.as_mut() {
            Some(ParameterType::UnsignedInteger(range)) => range,
            _ => panic!("unsigned shared parameter"),
        }
    }

    fn total(value: u64) -> ParameterValues {
        ParameterValues {
            values: vec![ParameterValue {
                parameter_id: TOTAL_CONTEXT_PARAMETER_ID.to_owned(),
                value: Some(Value::UnsignedInteger(value)),
            }],
        }
    }

    fn context_per_request(value: u64) -> ParameterValue {
        ParameterValue {
            parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
            value: Some(Value::UnsignedInteger(value)),
        }
    }
}
