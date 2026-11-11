//! Inference-owned engine integrations.

mod declaration;
mod definition;
mod instruction_roles;
mod probe;
mod realization;
mod registry;
mod speculative_draft;
pub(crate) mod startup_facts;
pub(crate) mod startup_refusal;

pub use declaration::EngineFacade;
pub(crate) use definition::{unsigned_parameter_range, unsigned_parameter_value};
pub(crate) use realization::{realize_model, validate_snapshot};
pub(crate) use registry::{
    EngineIntegrationError, EngineIntegrationRegistry, EngineTransportFactories,
};

pub(crate) mod llama_cpp;
pub(crate) mod loopback_http;
pub(crate) mod ninfer;
pub(crate) mod openai_compatible_transport;
pub(crate) mod strata;
mod system_one_transport;
pub(crate) mod vllm;

static ENGINE_FACADE: EngineFacade = EngineFacade;

/// Returns the backend's static engine declaration validator.
pub fn declaration_validator() -> &'static dyn dotcore_catalog::EngineDeclarationValidator {
    &ENGINE_FACADE
}

/// Returns the sets of auxiliary artifacts a configuration's options can add for a raw engine kind,
/// or none for an unsupported kind.
pub(crate) fn selectable_auxiliary_artifact_sets(
    kind: i32,
    configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
) -> Option<Vec<std::collections::BTreeSet<String>>> {
    definition::definition(kind)
        .map(|engine| engine.selectable_auxiliary_artifact_sets(configuration))
}

/// Returns the executable artifact kinds the engine of a raw engine kind launches from, or none
/// for an unsupported kind.
pub(crate) fn engine_artifact_kinds(kind: i32) -> &'static [dotcore_domain::WorkloadArtifactKind] {
    definition::definition(kind).map_or(&[], |engine| engine.artifact_kinds())
}

/// Returns the context per request resolution fills for values that leave it out, by the rule of
/// the engine of a raw engine kind; an unsupported kind takes the shared rule.
pub(crate) fn filled_context_per_request(
    kind: i32,
    schema: &dotcore_proto::dotcore::catalog::v1::EngineParameterSchema,
    values: &dotcore_proto::dotcore::catalog::v1::EngineParameterValues,
    maximum_context_tokens: u32,
) -> Option<dotcore_proto::dotcore::catalog::v1::EngineParameterValue> {
    match definition::definition(kind) {
        Some(engine) => engine.filled_context_per_request(schema, values, maximum_context_tokens),
        None => dotcore_catalog::shared_parameters::filled_context_per_request(
            schema,
            values,
            maximum_context_tokens,
        ),
    }
}

/// Rejects complete engine parameter values the engine of a raw engine kind cannot launch
/// together for the model's configuration: on every engine, a context per request above the total
/// context, then the engine's own rules. An unsupported kind accepts every other value.
pub(crate) fn validate_parameter_values(
    kind: i32,
    model: &dotcore_proto::dotcore::catalog::v1::Model,
    configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
    values: &dotcore_proto::dotcore::catalog::v1::EngineParameterValues,
) -> anyhow::Result<()> {
    validate_context_within_total(values)?;
    definition::definition(kind).map_or(Ok(()), |engine| {
        engine.validate_parameter_values(model, configuration, values)
    })
}

/// Rejects a context per request above the total context every request shares, since no request
/// could use it.
fn validate_context_within_total(
    values: &dotcore_proto::dotcore::catalog::v1::EngineParameterValues,
) -> anyhow::Result<()> {
    use dotcore_catalog::shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
    };

    let (Some(context_per_request), Some(total_context)) = (
        unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID),
        unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID),
    ) else {
        return Ok(());
    };
    if context_per_request > total_context {
        anyhow::bail!(
            "context per request of {context_per_request} tokens is above the total context of {total_context} tokens; lower the context per request or raise the total context"
        );
    }
    Ok(())
}
