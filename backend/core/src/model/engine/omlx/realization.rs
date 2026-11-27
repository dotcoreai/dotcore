//! Materializes the resolved oMLX engine selection from a model configuration.
//!
//! The retained state is the configuration's fixed settings. oMLX configurations declare no
//! options and bind no auxiliary artifacts, so nothing else is selected, and the input
//! capabilities are the configuration's own.

use dotcore_proto::dotcore::{
    catalog::v1::{Model, ModelArtifactBinding, ModelEngineConfiguration, ModelOptionValues},
    engine::{
        omlx::v1::OmlxResolvedModelState,
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
            Configuration::Omlx(payload) => Some(payload),
            Configuration::LlamaCpp(_)
            | Configuration::Vllm(_)
            | Configuration::Ninfer(_)
            | Configuration::Strata(_) => None,
        })
        .ok_or_else(|| anyhow::anyhow!("configuration has no oMLX payload"))?;
    let fixed_settings = payload
        .fixed_settings
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no fixed oMLX settings"))?;
    if !binding.auxiliary_artifact_ids.is_empty() {
        anyhow::bail!("oMLX binding selects auxiliary artifacts");
    }
    if !option_values.is_empty() {
        anyhow::bail!("oMLX configuration has no options to resolve");
    }
    let input_capabilities = configuration
        .input_capabilities
        .clone()
        .ok_or_else(|| anyhow::anyhow!("configuration has no input capabilities"))?;

    Ok(ResolvedEngineSelection {
        state: ResolvedEngineModelState {
            engine_kind: EngineKind::Omlx as i32,
            semantics_version: SEMANTICS_VERSION,
            state: Some(State::Omlx(OmlxResolvedModelState {
                fixed_settings: Some(fixed_settings),
            })),
        },
        auxiliary_artifact_ids: Vec::new(),
        input_capabilities,
    })
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::catalog::v1::ModelOptionParameterValue;

    use super::*;
    use crate::test_support::omlx::{configuration, model};

    #[test]
    fn realization_retains_the_fixed_settings_and_the_configuration_inputs() {
        let configuration = configuration();

        let selection = realize_model(
            &model(),
            &configuration,
            &configuration.artifact_bindings[0],
            &[],
        )
        .expect("realized selection");

        let Some(Configuration::Omlx(payload)) = configuration
            .engine_configuration
            .as_ref()
            .and_then(|payload| payload.configuration.clone())
        else {
            panic!("oMLX payload");
        };
        assert_eq!(
            selection.state,
            ResolvedEngineModelState {
                engine_kind: EngineKind::Omlx as i32,
                semantics_version: 1,
                state: Some(State::Omlx(OmlxResolvedModelState {
                    fixed_settings: payload.fixed_settings,
                })),
            }
        );
        assert_eq!(selection.auxiliary_artifact_ids, Vec::<String>::new());
        assert_eq!(
            Some(selection.input_capabilities),
            configuration.input_capabilities
        );
    }

    #[test]
    fn realization_rejects_auxiliary_artifacts_and_option_values() {
        let configuration = configuration();
        let mut binding = configuration.artifact_bindings[0].clone();
        binding.auxiliary_artifact_ids = vec!["draft".to_owned()];
        assert_eq!(
            realize_model(&model(), &configuration, &binding, &[])
                .err()
                .map(|error| error.to_string()),
            Some("oMLX binding selects auxiliary artifacts".to_owned())
        );

        let values = ModelOptionValues {
            option_id: "mtp".to_owned(),
            values: vec![ModelOptionParameterValue::default()],
        };
        assert_eq!(
            realize_model(
                &model(),
                &configuration,
                &configuration.artifact_bindings[0],
                &[values]
            )
            .err()
            .map(|error| error.to_string()),
            Some("oMLX configuration has no options to resolve".to_owned())
        );
    }
}
