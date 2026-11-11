//! Materializes the resolved Strata engine selection from a model configuration.
//!
//! The retained state is the configuration's fixed settings. Strata configurations declare no
//! options and Strata loads a single GGUF file, so no option values or auxiliary artifacts are
//! ever selected, and the input capabilities are the configuration's own.

use dotcore_proto::dotcore::{
    catalog::v1::{Model, ModelArtifactBinding, ModelEngineConfiguration, ModelOptionValues},
    engine::{
        strata::v1::StrataResolvedModelState,
        v1::{
            EngineKind, ResolvedEngineModelState, engine_model_configuration::Configuration,
            resolved_engine_model_state::State,
        },
    },
};

use crate::model::engine::realization::ResolvedEngineSelection;

pub(crate) const SEMANTICS_VERSION: u32 = 1;

pub(crate) fn realize_model(
    _model: &Model,
    configuration: &ModelEngineConfiguration,
    binding: &ModelArtifactBinding,
    option_values: &[ModelOptionValues],
) -> anyhow::Result<ResolvedEngineSelection> {
    let payload = configuration
        .engine_configuration
        .as_ref()
        .and_then(|payload| payload.configuration.as_ref())
        .and_then(|payload| match payload {
            Configuration::Strata(payload) => Some(payload),
            Configuration::LlamaCpp(_) | Configuration::Vllm(_) | Configuration::Ninfer(_) => None,
        })
        .ok_or_else(|| anyhow::anyhow!("configuration has no Strata payload"))?;
    let fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed Strata settings"))?;
    if !binding.auxiliary_artifact_ids.is_empty() {
        anyhow::bail!("Strata binding selects auxiliary artifacts");
    }
    if !option_values.is_empty() {
        anyhow::bail!("Strata configuration has no options to resolve");
    }
    let input_capabilities = configuration
        .input_capabilities
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no input capabilities"))?;

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::Strata as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::Strata(StrataResolvedModelState {
                fixed_settings: Some(fixed_settings),
            })),
        },
        auxiliary_artifact_ids: Vec::new(),
        input_capabilities,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::strata::{configuration, model};

    #[test]
    fn state_keeps_the_fixed_settings_and_the_configuration_input() {
        let configuration = configuration();

        let selection = realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[],
        )
        .expect("Strata selection");

        let Some(Configuration::Strata(payload)) = configuration
            .engine_configuration
            .clone()
            .and_then(|wrapper| wrapper.configuration)
        else {
            panic!("Strata payload");
        };
        assert_eq!(
            selection.state,
            ResolvedEngineModelState {
                engine_kind: EngineKind::Strata as i32,
                semantics_version: SEMANTICS_VERSION,
                state: Some(State::Strata(StrataResolvedModelState {
                    fixed_settings: payload.fixed_settings,
                })),
            }
        );
        assert_eq!(
            Some(selection.input_capabilities),
            configuration.input_capabilities
        );
        assert!(selection.auxiliary_artifact_ids.is_empty());
    }

    #[test]
    fn rejects_option_values_and_auxiliary_artifacts() {
        let configuration = configuration();
        let options = realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[ModelOptionValues {
                option_id: "mtp".to_owned(),
                values: Vec::new(),
            }],
        );
        assert_eq!(
            options.err().expect("option values").to_string(),
            "Strata configuration has no options to resolve"
        );

        let mut binding = configuration.artifact_bindings[0].clone();
        binding.auxiliary_artifact_ids.push("template".to_owned());
        assert_eq!(
            realize_model(&model(), &configuration, &binding, &[])
                .err()
                .expect("auxiliary artifact")
                .to_string(),
            "Strata binding selects auxiliary artifacts"
        );
    }
}
