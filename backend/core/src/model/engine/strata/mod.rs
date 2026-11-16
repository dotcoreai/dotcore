//! The Strata engine: catalog declaration validation, launch-plan translation, OCI workload
//! materialization, loopback readiness probing, and loopback inference binding.
//!
//! Strata serves one mixture-of-experts model on one NVIDIA GPU, keeping the experts that don't fit
//! on the GPU in host memory, through an OpenAI-compatible HTTP server started by Dotcore's keyless
//! start wrapper. A model is a single GGUF read in place, split GGUF shards Strata packs on first
//! start, or a ready-made pack, the last two with a separate draft layer. Callers reach it only
//! through its engine definition.

mod binding;
mod declaration;
mod plan;
mod readiness;
mod realization;
mod workload;

use std::sync::Arc;

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, EngineRelease, HardwareClass, InferenceRequestKind, Model,
        ModelArtifactBinding, ModelEngineConfiguration, ModelOptionValues, ParameterValue,
        ParameterValues,
    },
    engine::{
        strata::v1::{StrataFixedModelSettings, strata_fixed_model_settings::ModelFiles},
        v1::{EngineKind, ResolvedEngineModelState},
    },
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{definition::EngineDefinition, realization::ResolvedEngineSelection};
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

    fn filled_context_per_request(
        &self,
        schema: &EngineParameterSchema,
        values: &ParameterValues,
        maximum_context_tokens: u32,
    ) -> Option<ParameterValue> {
        declaration::filled_context_per_request(schema, values, maximum_context_tokens)
    }

    fn validate_parameter_values(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        values: &ParameterValues,
    ) -> anyhow::Result<()> {
        declaration::validate_parameter_values(model, configuration, values)
    }

    /// The expert accelerator memory cap comes from sizing, so every launch needs an estimate.
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
        Ok(plan::translate(snapshot)?)
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
    ) -> Arc<dyn EngineReadinessProbe> {
        Arc::new(readiness::StrataReadinessProbe::new(
            endpoint,
            served_model_id,
        ))
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::StrataTransportFactory)
    }
}

/// Whether fixed settings name an architecture the wrapper accepts, an MTP draft depth within its
/// range, and model files the wrapper can locate: a draft layer for the pack path, and plain
/// relative paths for a ready-made pack.
fn valid_fixed_settings(fixed: &StrataFixedModelSettings) -> bool {
    let model_files = match fixed.model_files.as_ref() {
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
        && !fixed.architecture.is_empty()
        && fixed
            .architecture
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && (1..=MAXIMUM_MTP_DRAFT_TOKENS).contains(&fixed.mtp_draft_tokens)
}

/// The model-local auxiliary artifact holding the draft layer, when the model files have one.
fn draft_layer_artifact_id(fixed: &StrataFixedModelSettings) -> Option<&str> {
    match fixed.model_files.as_ref()? {
        ModelFiles::SingleGguf(_) => None,
        ModelFiles::SplitGguf(split) => Some(&split.draft_layer_auxiliary_artifact_id),
        ModelFiles::ReadyPack(pack) => Some(&pack.draft_layer_auxiliary_artifact_id),
    }
}

/// Whether `path` names a file or folder below an artifact's root without leaving it. The wrapper
/// parses its arguments with argparse, so a path starting with '-' would read as an option.
fn plain_relative_path(path: &str) -> bool {
    !path.starts_with('-')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
