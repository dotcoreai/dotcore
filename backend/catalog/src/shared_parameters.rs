//! The engine parameters every engine release shares: total context, context per request and
//! parallel requests.
//!
//! The catalog owns their ids and their fixed shape and visibility, so controllers, adapters and
//! the console name and place them the same way on every engine, while each release sets its own
//! ranges, default values and descriptions. It also derives the context values a deployment
//! leaves out, by each engine's rule for how they relate.

use dotcore_proto::dotcore::catalog::v1::{
    EngineParameterSchema, Model, ModelEngineConfiguration, ParameterDefinition, ParameterValue,
    ParameterValues, ParameterVisibility, parameter_definition::ParameterType,
    parameter_value::Value,
};

use crate::typed_parameters::is_declared_visibility;

/// Id of the total context: the KV pool an engine holds at start, shared by active requests.
pub const TOTAL_CONTEXT_PARAMETER_ID: &str = "total-context";

/// Id of the context per request: the largest context one request may use.
pub const CONTEXT_PER_REQUEST_PARAMETER_ID: &str = "context-per-request";

/// Id of the parallel requests: requests an engine generates at once, while others queue.
pub const PARALLEL_REQUESTS_PARAMETER_ID: &str = "parallel-requests";

/// Dotcore's context floor, in tokens: the smallest total context and context per request any
/// engine release may accept. Below it no engine serves a useful request, so it is a product
/// decision rather than an engine limit; a release may still declare a higher minimum of its own.
pub const CONTEXT_FLOOR_TOKENS: u64 = 512;

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

/// Context values resolution derives for values that leave them out, by the engine's `rule`, in
/// shared parameter order. With neither context value, the context per request starts at its
/// engine default capped by the maximum context. A total context left out follows the context per
/// request and the parallel requests; a context per request left out beside a total context is
/// filled from it. Nothing is derived for a context value the schema doesn't declare or the values
/// already supply.
pub fn derived_context_values(
    schema: &EngineParameterSchema,
    values: &ParameterValues,
    maximum_context_tokens: u32,
    rule: &dyn ContextRule,
) -> Vec<ParameterValue> {
    let declared = |parameter_id: &str| {
        schema
            .parameters
            .iter()
            .any(|definition| definition.parameter_id == parameter_id)
    };
    let maximum = u64::from(maximum_context_tokens);
    let parallel_requests = unsigned_value(values, PARALLEL_REQUESTS_PARAMETER_ID)
        .or_else(|| engine_default(schema, PARALLEL_REQUESTS_PARAMETER_ID))
        .unwrap_or(1)
        .max(1);
    let supplied_per_request = unsigned_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID);
    let supplied_total = unsigned_value(values, TOTAL_CONTEXT_PARAMETER_ID);

    let mut derived = Vec::new();
    match (supplied_per_request, supplied_total) {
        (Some(_), Some(_)) => {}
        (None, Some(total_context)) => {
            if declared(CONTEXT_PER_REQUEST_PARAMETER_ID) {
                derived.push(unsigned_parameter(
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    rule.filled_context_per_request(total_context, parallel_requests, maximum),
                ));
            }
        }
        (supplied_per_request, None) => {
            // The engine default lives on the context per request's definition, so an undeclared
            // context per request has none.
            let Some(context_per_request) = supplied_per_request.or_else(|| {
                engine_default(schema, CONTEXT_PER_REQUEST_PARAMETER_ID)
                    .map(|default| default.min(maximum))
            }) else {
                return derived;
            };
            if supplied_per_request.is_none() {
                derived.push(unsigned_parameter(
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    context_per_request,
                ));
            }
            if declared(TOTAL_CONTEXT_PARAMETER_ID) {
                derived.push(unsigned_parameter(
                    TOTAL_CONTEXT_PARAMETER_ID,
                    rule.derived_total_context(context_per_request, parallel_requests, maximum),
                ));
            }
        }
    }
    derived
}

/// How one engine relates its total context to its context per request and parallel requests.
/// The provided methods are the shared rule, for engines whose requests share one total context;
/// an engine overrides the part its launch relates differently.
pub trait ContextRule: Sync {
    /// Context per request filled from a supplied total context: the smaller of the total context
    /// and the maximum context.
    fn filled_context_per_request(
        &self,
        total_context: u64,
        _parallel_requests: u64,
        maximum_context_tokens: u64,
    ) -> u64 {
        total_context.min(maximum_context_tokens)
    }

    /// Total context derived from a context per request: room for every parallel request to use
    /// that context at once.
    fn derived_total_context(
        &self,
        context_per_request: u64,
        parallel_requests: u64,
        _maximum_context_tokens: u64,
    ) -> u64 {
        context_per_request.saturating_mul(parallel_requests)
    }
}

/// The shared context rule, for engines whose requests share one total context.
pub struct SharedContextRule;

impl ContextRule for SharedContextRule {}

/// Whether resolution applies the parameter's engine default rather than the default merge. The
/// context per request's applies only when neither context value is given, so it never takes the
/// place of a context per request filled from a total context.
pub fn resolution_applies_default(parameter_id: &str) -> bool {
    parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID
}

/// Whether a profile may set a default for the parameter. Profiles carry no context: the
/// deployment or the engine default sets the context per request, and resolution derives the
/// total context or fills the context per request.
pub(crate) fn profile_may_default(parameter_id: &str) -> bool {
    SHARED_PARAMETERS
        .iter()
        .find(|shared| shared.parameter_id == parameter_id)
        .is_none_or(|shared| shared.profile_default)
}

/// Whether the parameter always sits in the main view: the shared parameters do, and a profile
/// may only hide them.
fn in_main_view(parameter_id: &str) -> bool {
    SHARED_PARAMETERS.iter().any(|shared| {
        shared.parameter_id == parameter_id && shared.visibility == ParameterVisibility::Basic
    })
}

/// Returns why a profile may not give the parameter this visibility, or none when it may. The
/// shared parameters belong to the main view: a profile may hide them but not move them to the
/// advanced view. A profile may give any other parameter any visibility.
pub(crate) fn profile_visibility_problem(
    parameter_id: &str,
    visibility: ParameterVisibility,
) -> Option<&'static str> {
    if !is_declared_visibility(visibility) {
        return Some("requires a basic, advanced or hidden visibility");
    }
    if visibility == ParameterVisibility::Advanced && in_main_view(parameter_id) {
        return Some("may be hidden but not advanced");
    }
    None
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
        visibility: ParameterVisibility::Basic,
        minimum_floor: CONTEXT_FLOOR_TOKENS,
        engine_default: false,
        profile_default: false,
    },
    SharedParameter {
        parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID,
        display_name: "Context per request",
        unit: "tokens",
        visibility: ParameterVisibility::Basic,
        minimum_floor: CONTEXT_FLOOR_TOKENS,
        engine_default: true,
        profile_default: false,
    },
    SharedParameter {
        parameter_id: PARALLEL_REQUESTS_PARAMETER_ID,
        display_name: "Parallel requests",
        unit: "requests",
        visibility: ParameterVisibility::Basic,
        // Zero is never a usable count.
        minimum_floor: 1,
        engine_default: true,
        profile_default: true,
    },
];

/// One shared parameter's fixed shape. Every shared parameter is an unsigned integer the console
/// shows in its main view.
struct SharedParameter {
    parameter_id: &'static str,
    display_name: &'static str,
    unit: &'static str,
    visibility: ParameterVisibility,

    /// Smallest minimum a release may declare; each release declares its engine's own minimum at
    /// or above it.
    minimum_floor: u64,

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
        if range.minimum < self.minimum_floor {
            return Some(format!(
                "must have a minimum of at least {}",
                self.minimum_floor
            ));
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

/// Returns the parameter's unsigned value, when the values hold one.
fn unsigned_value(values: &ParameterValues, parameter_id: &str) -> Option<u64> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value {
            Some(Value::UnsignedInteger(value)) => Some(value),
            _ => None,
        })
}

/// Returns the unsigned engine default the schema declares for the parameter.
fn engine_default(schema: &EngineParameterSchema, parameter_id: &str) -> Option<u64> {
    schema
        .parameters
        .iter()
        .find(|definition| definition.parameter_id == parameter_id)
        .and_then(|definition| match definition.parameter_type.as_ref() {
            Some(ParameterType::UnsignedInteger(range)) => range.default_value,
            _ => None,
        })
}

fn unsigned_parameter(parameter_id: &str, value: u64) -> ParameterValue {
    ParameterValue {
        parameter_id: parameter_id.to_owned(),
        value: Some(Value::UnsignedInteger(value)),
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
    fn each_release_may_declare_its_own_context_minimum_above_the_floor() {
        let mut schema = shared_schema();
        for definition in &mut schema.parameters {
            if definition.parameter_id != PARALLEL_REQUESTS_PARAMETER_ID {
                range(definition).minimum = 1024;
            }
        }

        assert!(validate_shared_parameters(&schema).is_empty());
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
        let cases: [(&str, ShapeChange, &str); 11] = [
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
                |definition| definition.set_visibility(ParameterVisibility::Advanced),
                "must be basic",
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
                TOTAL_CONTEXT_PARAMETER_ID,
                |definition| range(definition).minimum = 511,
                "must have a minimum of at least 512",
            ),
            (
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                |definition| range(definition).minimum = 256,
                "must have a minimum of at least 512",
            ),
            (
                PARALLEL_REQUESTS_PARAMETER_ID,
                |definition| range(definition).minimum = 0,
                "must have a minimum of at least 1",
            ),
            (
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                |definition| range(definition).default_value = None,
                "requires an engine default",
            ),
            (
                TOTAL_CONTEXT_PARAMETER_ID,
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
            parameter_groups: Vec::new(),
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
    fn profiles_may_hide_any_parameter_and_keep_the_shared_parameters_basic() {
        use ParameterVisibility::{Advanced, Basic, Hidden, Unspecified};

        for parameter_id in [
            TOTAL_CONTEXT_PARAMETER_ID,
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            PARALLEL_REQUESTS_PARAMETER_ID,
        ] {
            assert_eq!(profile_visibility_problem(parameter_id, Basic), None);
            assert_eq!(profile_visibility_problem(parameter_id, Hidden), None);
            assert_eq!(
                profile_visibility_problem(parameter_id, Advanced),
                Some("may be hidden but not advanced")
            );
        }
        assert_eq!(
            profile_visibility_problem("prompt-cache-memory-mib", Basic),
            None
        );
        assert_eq!(
            profile_visibility_problem("prompt-cache-memory-mib", Advanced),
            None
        );
        assert_eq!(
            profile_visibility_problem("prompt-cache-memory-mib", Hidden),
            None
        );
        assert_eq!(
            profile_visibility_problem(TOTAL_CONTEXT_PARAMETER_ID, Unspecified),
            Some("requires a basic, advanced or hidden visibility")
        );
    }

    #[test]
    fn only_the_context_per_request_default_is_left_to_resolution() {
        assert!(resolution_applies_default(CONTEXT_PER_REQUEST_PARAMETER_ID));
        assert!(!resolution_applies_default(TOTAL_CONTEXT_PARAMETER_ID));
        assert!(!resolution_applies_default(PARALLEL_REQUESTS_PARAMETER_ID));
        assert!(!resolution_applies_default("prompt-cache-memory-mib"));
    }

    #[test]
    fn a_total_context_alone_fills_the_smaller_of_the_maximum_and_the_total_context() {
        let schema = shared_schema();
        let derived = |total_context: u64, maximum: u32| {
            derived_context_values(
                &schema,
                &values(&[(TOTAL_CONTEXT_PARAMETER_ID, total_context)]),
                maximum,
                &SharedContextRule,
            )
        };

        assert_eq!(
            derived(32_768, 262_144),
            [unsigned_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768)]
        );
        assert_eq!(
            derived(1_048_576, 262_144),
            [unsigned_parameter(
                CONTEXT_PER_REQUEST_PARAMETER_ID,
                262_144
            )]
        );
    }

    #[test]
    fn a_context_per_request_alone_derives_the_total_context_for_the_parallel_requests() {
        let schema = shared_schema();

        assert_eq!(
            derived_context_values(
                &schema,
                &values(&[
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 3),
                ]),
                262_144,
                &SharedContextRule,
            ),
            [unsigned_parameter(TOTAL_CONTEXT_PARAMETER_ID, 49_152)]
        );
        // Parallel requests left out take the engine default.
        assert_eq!(
            derived_context_values(
                &schema,
                &values(&[(CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384)]),
                262_144,
                &SharedContextRule,
            ),
            [unsigned_parameter(TOTAL_CONTEXT_PARAMETER_ID, 65_536)]
        );
    }

    #[test]
    fn no_context_value_starts_the_context_per_request_at_the_engine_default_capped_by_the_maximum()
    {
        let schema = shared_schema();
        let derived = |maximum: u32| {
            derived_context_values(
                &schema,
                &values(&[(PARALLEL_REQUESTS_PARAMETER_ID, 2)]),
                maximum,
                &SharedContextRule,
            )
        };

        assert_eq!(
            derived(262_144),
            [
                unsigned_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
                unsigned_parameter(TOTAL_CONTEXT_PARAMETER_ID, 65_536),
            ]
        );
        assert_eq!(
            derived(16_384),
            [
                unsigned_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384),
                unsigned_parameter(TOTAL_CONTEXT_PARAMETER_ID, 32_768),
            ]
        );
    }

    #[test]
    fn an_engine_rule_takes_the_place_of_the_shared_fill_and_total() {
        struct Split;
        impl ContextRule for Split {
            fn filled_context_per_request(&self, total: u64, parallel: u64, maximum: u64) -> u64 {
                (total / parallel).min(maximum)
            }

            fn derived_total_context(&self, per_request: u64, parallel: u64, maximum: u64) -> u64 {
                per_request.saturating_mul(parallel).min(maximum)
            }
        }
        let schema = shared_schema();

        assert_eq!(
            derived_context_values(
                &schema,
                &values(&[
                    (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 4),
                ]),
                262_144,
                &Split,
            ),
            [unsigned_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384)]
        );
        assert_eq!(
            derived_context_values(
                &schema,
                &values(&[(PARALLEL_REQUESTS_PARAMETER_ID, 4)]),
                65_536,
                &Split,
            ),
            [
                unsigned_parameter(CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
                unsigned_parameter(TOTAL_CONTEXT_PARAMETER_ID, 65_536),
            ]
        );
    }

    #[test]
    fn supplied_or_undeclared_context_values_are_not_derived() {
        let schema = shared_schema();
        let undeclared = EngineParameterSchema {
            schema_id: "fixture-other".to_owned(),
            parameters: vec![definition("memory-bytes", "Memory", "bytes", 1, None)],
            parameter_groups: Vec::new(),
        };

        assert_eq!(
            derived_context_values(
                &schema,
                &values(&[
                    (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 8192),
                ]),
                262_144,
                &SharedContextRule,
            ),
            []
        );
        assert_eq!(
            derived_context_values(
                &undeclared,
                &values(&[(TOTAL_CONTEXT_PARAMETER_ID, 32_768)]),
                262_144,
                &SharedContextRule,
            ),
            []
        );
        assert_eq!(
            derived_context_values(
                &undeclared,
                &ParameterValues::default(),
                262_144,
                &SharedContextRule,
            ),
            []
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
                    512,
                    None,
                ),
                definition(
                    CONTEXT_PER_REQUEST_PARAMETER_ID,
                    "Context per request",
                    "tokens",
                    512,
                    Some(32_768),
                ),
                definition(
                    PARALLEL_REQUESTS_PARAMETER_ID,
                    "Parallel requests",
                    "requests",
                    1,
                    Some(4),
                ),
            ],
            parameter_groups: Vec::new(),
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
            summary: "Summary.".into(),
            parameter_id: parameter_id.to_owned(),
            display_name: display_name.to_owned(),
            parameter_type: Some(ParameterType::UnsignedInteger(
                UnsignedIntegerParameterSchema {
                    minimum,
                    maximum: Some(16_777_216),
                    unit: unit.to_owned(),
                    default_value,
                    memory_pool: None,
                },
            )),
            visibility: SHARED_PARAMETERS
                .iter()
                .find(|shared| shared.parameter_id == parameter_id)
                .map_or(ParameterVisibility::Advanced, |shared| shared.visibility)
                as i32,
            description: String::new(),
            enabled_by_parameter_id: None,
            group_id: None,
        }
    }

    fn range(definition: &mut ParameterDefinition) -> &mut UnsignedIntegerParameterSchema {
        match definition.parameter_type.as_mut() {
            Some(ParameterType::UnsignedInteger(range)) => range,
            _ => panic!("unsigned shared parameter"),
        }
    }

    fn values(values: &[(&str, u64)]) -> ParameterValues {
        ParameterValues {
            values: values
                .iter()
                .map(|(parameter_id, value)| unsigned_parameter(parameter_id, *value))
                .collect(),
        }
    }
}
