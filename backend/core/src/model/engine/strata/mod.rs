//! The Strata engine: catalog declaration validation, launch-plan translation, OCI workload
//! materialization, loopback readiness probing, and loopback inference binding.
//!
//! Strata serves one mixture-of-experts model on one NVIDIA GPU, keeping the experts that don't fit
//! on the GPU in host memory, through an OpenAI-compatible HTTP server started by Dotcore's keyless
//! start wrapper. Its main engine runs a single GGUF read in place, split GGUF shards Strata packs
//! on first start, or a ready-made pack, the last two with a separate draft layer; its Inkling
//! engine runs one native Inkling model file, a ready-made pack or a GGUF, with a separate tokenizer
//! and, while drafting is on, a separate drafter. Callers reach it only through its engine
//! definition.

mod binding;
mod declaration;
mod plan;
mod readiness;
mod realization;
mod workload;

use std::sync::Arc;

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_catalog::shared_parameters::ContextRule;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, HardwareClass, InferenceRequestKind, Model, ModelArtifactBinding,
        ModelEngineConfiguration, ModelOptionValues, ParameterValues,
        hugging_face_artifact_source::Content as HuggingFaceContent,
    },
    engine::{
        strata::v1::{
            StrataFixedModelSettings, StrataInklingEngine, StrataMainEngine,
            strata_fixed_model_settings::EnginePath, strata_main_engine::ModelFiles,
        },
        v1::{EngineKind, ResolvedEngineModelState},
    },
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{
    definition::{CustomArgumentConflict, EngineDefinition},
    realization::ResolvedEngineSelection,
};
use crate::{
    inference::{EngineReadinessProbe, EngineTransportFactory},
    model::resolved::ResolvedModelLaunch,
};
#[cfg(test)]
pub(crate) use plan::translate;
#[cfg(test)]
pub(crate) use realization::realize_model;

/// The Strata engine.
pub(crate) struct Strata;

/// Container port Strata's server listens on for both its API and its browser UI.
const SERVICE_PORT: u16 = 8080;

/// Largest MTP draft depth Strata's start wrapper accepts.
const MAXIMUM_MTP_DRAFT_TOKENS: u32 = 8;

/// File extensions of the native model files Strata's Inkling engine reads: a pack and a GGUF.
const INKLING_MODEL_EXTENSIONS: [&str; 2] = [".strata", ".gguf"];

/// Inference request kinds Strata serves: chat only.
const REQUEST_KINDS: &[InferenceRequestKind] = &[InferenceRequestKind::Chat];

impl EngineDefinition for Strata {
    fn kind(&self) -> EngineKind {
        EngineKind::Strata
    }

    fn name(&self) -> &'static str {
        "Strata"
    }

    fn request_kinds(&self) -> &'static [InferenceRequestKind] {
        REQUEST_KINDS
    }

    fn validate_engine_release(&self, engine: &EngineRelease) -> Vec<anyhow::Error> {
        declaration::validate_engine_release(engine)
    }

    fn validate_model_configuration(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        engine: &EngineRelease,
    ) -> Vec<anyhow::Error> {
        declaration::validate_model_configuration(model, configuration, engine)
    }

    fn validate_hardware_support(
        &self,
        configuration: &ModelEngineConfiguration,
        hardware_classes: &[&HardwareClass],
    ) -> Vec<anyhow::Error> {
        declaration::validate_hardware_support(configuration, hardware_classes)
    }

    fn context_rule(&self, _configuration: &ModelEngineConfiguration) -> &'static dyn ContextRule {
        &declaration::StrataContextRule
    }

    fn validate_parameter_values(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        values: &ParameterValues,
    ) -> anyhow::Result<()> {
        declaration::validate_parameter_values(model, configuration, values)
    }

    /// The main engine's expert accelerator memory cap comes from sizing, and the Inkling engine
    /// takes whatever accelerator memory it finds, so every launch needs an estimate of its use.
    fn launch_requires_sizing(&self) -> bool {
        true
    }

    fn realize_model(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        binding: &ModelArtifactBinding,
        option_values: &[ModelOptionValues],
    ) -> anyhow::Result<ResolvedEngineSelection> {
        realization::realize_model(model, configuration, binding, option_values)
    }

    fn selectable_auxiliary_artifact_sets(
        &self,
        configuration: &ModelEngineConfiguration,
    ) -> Vec<std::collections::BTreeSet<String>> {
        realization::selectable_auxiliary_artifact_sets(configuration)
    }

    fn validate_snapshot(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<()> {
        Ok(plan::validate_snapshot(snapshot)?)
    }

    fn translate(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<PortableModelLaunchPlan> {
        plan::translate(snapshot).map_err(|error| match error {
            plan::StrataPlanError::CustomArgument(flag) => {
                CustomArgumentConflict::unsupported("Strata", &flag).into()
            }
            error => error.into(),
        })
    }

    /// Strata keeps every request's context on the accelerator.
    fn enables_host_kv_offload(
        &self,
        _snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<bool> {
        Ok(false)
    }

    fn artifact_kinds(&self) -> &'static [WorkloadArtifactKind] {
        workload::ARTIFACT_KINDS
    }

    fn workload(
        &self,
        model: &ResolvedModelLaunch,
        target: &ExecutionTarget,
        artifacts: &PreparedModelArtifacts,
    ) -> anyhow::Result<WorkloadSpec> {
        Ok(workload::workload(model, target, artifacts)?)
    }

    fn readiness_probe(
        &self,
        endpoint: LoopbackEndpoint,
        served_model_id: String,
        _resolved_engine_state: &ResolvedEngineModelState,
        context_per_request: Option<u64>,
    ) -> Arc<dyn EngineReadinessProbe> {
        Arc::new(readiness::StrataReadinessProbe::new(
            endpoint,
            served_model_id,
            context_per_request,
        ))
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::StrataTransportFactory)
    }
}

/// Whether fixed settings name an engine path the wrapper accepts: for the main engine, an
/// architecture it accepts, an MTP draft depth within its range, and model files it can locate (a
/// draft layer for the pack path, plain relative paths for a ready-made pack). The Inkling engine
/// takes its model file from the bound model artifact, so its settings only need a tokenizer.
fn valid_fixed_settings(fixed: &StrataFixedModelSettings) -> bool {
    match fixed.engine_path.as_ref() {
        Some(EnginePath::MainEngine(main)) => valid_main_engine(main),
        Some(EnginePath::InklingEngine(inkling)) => {
            !inkling.tokenizer_auxiliary_artifact_id.is_empty()
        }
        None => false,
    }
}

/// The model-local auxiliary artifact every binding selects: the draft layer on the main engine's
/// pack path, the tokenizer on the Inkling engine.
fn bound_auxiliary_artifact_id(fixed: &StrataFixedModelSettings) -> Option<&str> {
    draft_layer_artifact_id(fixed).or_else(|| {
        inkling_engine(fixed).map(|inkling| inkling.tokenizer_auxiliary_artifact_id.as_str())
    })
}

/// The main engine settings of fixed settings, when the main engine runs the model.
fn main_engine(fixed: &StrataFixedModelSettings) -> Option<&StrataMainEngine> {
    match fixed.engine_path.as_ref()? {
        EnginePath::MainEngine(main) => Some(main),
        EnginePath::InklingEngine(_) => None,
    }
}

/// The Inkling engine settings of fixed settings, when the Inkling engine runs the model.
fn inkling_engine(fixed: &StrataFixedModelSettings) -> Option<&StrataInklingEngine> {
    match fixed.engine_path.as_ref()? {
        EnginePath::InklingEngine(inkling) => Some(inkling),
        EnginePath::MainEngine(_) => None,
    }
}

/// The model-local auxiliary artifact holding the draft layer, when the model files have one.
fn draft_layer_artifact_id(fixed: &StrataFixedModelSettings) -> Option<&str> {
    match main_engine(fixed)?.model_files.as_ref()? {
        ModelFiles::SingleGguf(_) => None,
        ModelFiles::SplitGguf(split) => Some(&split.draft_layer_auxiliary_artifact_id),
        ModelFiles::ReadyPack(pack) => Some(&pack.draft_layer_auxiliary_artifact_id),
    }
}

fn valid_main_engine(main: &StrataMainEngine) -> bool {
    let model_files = match main.model_files.as_ref() {
        Some(ModelFiles::SingleGguf(_)) => true,
        Some(ModelFiles::SplitGguf(split)) => !split.draft_layer_auxiliary_artifact_id.is_empty(),
        Some(ModelFiles::ReadyPack(pack)) => {
            !pack.draft_layer_auxiliary_artifact_id.is_empty()
                && [
                    &pack.pack_path,
                    &pack.dense_gguf_path,
                    &pack.lookup_table_gguf_path,
                    &pack.token_embedding_gguf_path,
                ]
                .into_iter()
                .all(|path| plain_relative_path(path))
        }
        None => false,
    };
    model_files
        && !main.architecture.is_empty()
        && main
            .architecture
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && (1..=MAXIMUM_MTP_DRAFT_TOKENS).contains(&main.mtp_draft_tokens)
}

/// The native Inkling model file within a model artifact: its only `.strata` or `.gguf` file, at a
/// plain relative path. None when the artifact isn't a snapshot or holds no such file, or several.
fn inkling_model_path(content: Option<&HuggingFaceContent>) -> Option<&str> {
    let Some(HuggingFaceContent::Snapshot(snapshot)) = content else {
        return None;
    };
    let mut models = snapshot.files.iter().filter_map(|file| {
        let extension = INKLING_MODEL_EXTENSIONS
            .into_iter()
            .find(|extension| file.path.ends_with(extension))?;
        Some((file.path.as_str(), extension))
    });
    let (model, extension) = models.next()?;
    let name = model.rsplit('/').next().unwrap_or_default();
    (models.next().is_none() && plain_relative_path(model) && name.len() > extension.len())
        .then_some(model)
}

/// Whether `path` names a file or folder below an artifact's root without leaving it. The wrapper
/// parses its arguments with argparse, so a path starting with '-' would read as an option.
fn plain_relative_path(path: &str) -> bool {
    !path.starts_with('-')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
