//! Engine option realization and retained snapshot dispatch.

use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, Model, ModelArtifactBinding, ModelEngineConfiguration,
        ModelInputCapabilities, ModelOptionValues,
    },
    engine::v1::ResolvedEngineModelState,
    model::v1::ResolvedLocalModelDeploymentSnapshot,
};

use super::definition::definition;

/// Concrete engine effects selected from generic option values.
pub(crate) struct ResolvedEngineSelection {
    pub(crate) state: ResolvedEngineModelState,
    pub(crate) auxiliary_artifact_ids: Vec<String>,
    pub(crate) input_capabilities: ModelInputCapabilities,
}

pub(crate) fn realize_model(
    release: &EngineRelease,
    model: &Model,
    configuration: &ModelEngineConfiguration,
    binding: &ModelArtifactBinding,
    option_values: &[ModelOptionValues],
) -> anyhow::Result<ResolvedEngineSelection> {
    let Some(definition) = definition(release.engine_kind) else {
        anyhow::bail!("cannot realize an unsupported engine kind")
    };
    definition.realize_model(model, configuration, binding, option_values)
}

pub(crate) fn validate_snapshot(
    local: &ResolvedLocalModelDeploymentSnapshot,
) -> anyhow::Result<()> {
    let Some(definition) = local
        .engine_artifact
        .as_ref()
        .and_then(|engine| definition(engine.engine_kind))
    else {
        anyhow::bail!("cannot validate a snapshot for an unsupported engine kind")
    };
    definition.validate_snapshot(local)
}
