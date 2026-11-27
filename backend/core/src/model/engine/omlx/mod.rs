//! The oMLX engine: catalog declaration validation, launch-plan translation, native Nix workload
//! materialization, loopback readiness probing, loopback inference binding, and startup refusals.
//!
//! oMLX serves MLX-format weights from one pinned Hugging Face snapshot on an Apple Silicon Mac as
//! a native host process, started through its package launcher, which writes oMLX's own settings
//! into the workload's working directory. oMLX manages its own memory under a whole-process
//! ceiling derived from the deployment's estimate, and grows into that estimate as requests arrive.
//! Callers reach it only through its engine definition.

mod binding;
mod declaration;
mod plan;
mod readiness;
mod realization;
mod startup_refusal;
mod workload;

use std::sync::Arc;

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_catalog::shared_parameters::ContextRule;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, HardwareClass, InferenceRequestKind, Model, ModelArtifactBinding,
        ModelEngineConfiguration, ModelOptionValues, ParameterValues,
    },
    engine::v1::{EngineKind, ResolvedEngineModelState},
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{
    definition::{CustomArgumentConflict, EngineDefinition},
    realization::ResolvedEngineSelection,
    startup_refusal::StartupRefusal,
};
use crate::{
    inference::{EngineReadinessProbe, EngineTransportFactory},
    model::resolved::ResolvedModelLaunch,
};
#[cfg(test)]
pub(crate) use plan::translate;
#[cfg(test)]
pub(crate) use realization::realize_model;

/// The oMLX engine.
pub(crate) struct Omlx;

/// Inference request kinds oMLX serves: chat only.
const REQUEST_KINDS: &[InferenceRequestKind] = &[InferenceRequestKind::Chat];

impl EngineDefinition for Omlx {
    fn kind(&self) -> EngineKind {
        EngineKind::Omlx
    }

    fn name(&self) -> &'static str {
        "oMLX"
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
        &declaration::OmlxContextRule
    }

    fn validate_parameter_values(
        &self,
        _model: &Model,
        _configuration: &ModelEngineConfiguration,
        values: &ParameterValues,
    ) -> anyhow::Result<()> {
        declaration::validate_parameter_values(values)
    }

    /// oMLX's memory ceiling comes from the deployment's estimate, so every launch needs one.
    fn launch_requires_sizing(&self) -> bool {
        true
    }

    /// oMLX allocates its KV cache and prompt cache as requests arrive, so a ready deployment holds
    /// only its weights until load fills the rest of its estimate.
    fn grows_into_estimate_after_ready(&self) -> bool {
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
        plan::translate(snapshot).map_err(|error| match error {
            plan::OmlxPlanError::CustomArgument(flag) => {
                CustomArgumentConflict::unsupported("oMLX", &flag).into()
            }
            error => error.into(),
        })
    }

    /// oMLX keeps no KV cache outside the unified memory pool it shares with the host.
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
        Arc::new(readiness::OmlxReadinessProbe::new(
            endpoint,
            served_model_id,
            context_per_request,
        ))
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::OmlxTransportFactory)
    }

    fn startup_refusal(&self, output: &str) -> Option<StartupRefusal> {
        startup_refusal::startup_refusal(output)
    }
}
