//! The llama.cpp engine: Catalog declaration validation, launch-plan translation, OCI and Nix
//! workload materialization, loopback readiness probing, and loopback inference binding.
//!
//! llama.cpp serves GGUF model artifacts and may keep KV cache in host memory. Callers reach the
//! engine only through its engine definition.

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
        EngineParameterValues, EngineRelease, HardwareClass, InferenceRequestKind, Model,
        ModelArtifactBinding, ModelEngineConfiguration, ModelOptionValues,
    },
    engine::v1::{EngineKind, ResolvedEngineModelState},
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{definition::EngineDefinition, realization::ResolvedEngineSelection};
use crate::{
    inference::{EngineReadinessProbe, EngineTransportFactory},
    model::resolved::ResolvedModelLaunch,
};

/// The llama.cpp engine.
pub(crate) struct LlamaCpp;

/// Inference request kinds llama.cpp serves: chat and decisions (System One).
const REQUEST_KINDS: &[InferenceRequestKind] =
    &[InferenceRequestKind::Chat, InferenceRequestKind::Decision];

impl EngineDefinition for LlamaCpp {
    fn kind(&self) -> EngineKind {
        EngineKind::LlamaCpp
    }

    fn name(&self) -> &'static str {
        "llama.cpp"
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
        let mut errors = declaration::validate_model_configuration(model, configuration);
        errors.extend(declaration::validate_total_context_default(
            model,
            configuration,
            engine,
        ));
        errors
    }

    fn validate_hardware_support(
        &self,
        configuration: &ModelEngineConfiguration,
        hardware_classes: &[&HardwareClass],
    ) -> Vec<anyhow::Error> {
        declaration::validate_hardware_support(configuration, hardware_classes)
    }

    fn parameter_defaults(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
    ) -> EngineParameterValues {
        declaration::parameter_defaults(model, configuration)
    }

    fn validate_parameter_values(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        values: &EngineParameterValues,
    ) -> anyhow::Result<()> {
        declaration::validate_parameter_values(model, configuration, values)
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
        Ok(plan::translate(snapshot)?)
    }

    fn enables_host_kv_offload(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<bool> {
        Ok(plan::enables_host_kv_offload(snapshot))
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
        Arc::new(readiness::LlamaCppReadinessProbe::new(
            endpoint,
            served_model_id,
        ))
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::LlamaCppTransportFactory)
    }
}
