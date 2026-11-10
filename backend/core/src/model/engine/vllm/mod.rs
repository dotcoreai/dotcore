//! The vLLM engine: Catalog declaration validation, launch-plan translation, OCI workload
//! materialization, loopback readiness probing, loopback inference binding, startup memory
//! facts, and startup memory refusals.
//!
//! vLLM launches require catalog sizing, and readiness follows the configuration's declared
//! readiness policy. Callers reach the engine only through its engine definition.

mod binding;
mod declaration;
mod plan;
mod readiness;
mod realization;
mod startup_facts;
mod startup_refusal;
mod workload;

use std::sync::Arc;

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, HardwareClass, InferenceRequestKind, Model, ModelArtifactBinding,
        ModelEngineConfiguration, ModelOptionValues,
    },
    engine::v1::{EngineKind, ResolvedEngineModelState, resolved_engine_model_state::State},
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{
    definition::EngineDefinition, realization::ResolvedEngineSelection,
    startup_facts::StartupFactExtractor, startup_refusal::StartupRefusal,
};
use crate::{
    inference::{EngineBindingError, EngineReadinessProbe, EngineTransportFactory},
    model::resolved::ResolvedModelLaunch,
};

/// The vLLM engine.
pub(crate) struct Vllm;

/// Inference request kinds vLLM serves: chat only.
const REQUEST_KINDS: &[InferenceRequestKind] = &[InferenceRequestKind::Chat];

impl EngineDefinition for Vllm {
    fn kind(&self) -> EngineKind {
        EngineKind::Vllm
    }

    fn name(&self) -> &'static str {
        "vLLM"
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
        Ok(plan::translate(snapshot)?)
    }

    fn enables_host_kv_offload(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<bool> {
        Ok(plan::enables_host_kv_offload(snapshot)?)
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
        resolved_engine_state: &ResolvedEngineModelState,
    ) -> Arc<dyn EngineReadinessProbe> {
        Arc::new(readiness::VllmReadinessProbe::new(
            endpoint,
            served_model_id,
            readiness_mode(resolved_engine_state).unwrap_or_default(),
        ))
    }

    fn validate_binding_state(
        &self,
        resolved_engine_state: &ResolvedEngineModelState,
    ) -> Result<(), EngineBindingError> {
        match readiness_mode(resolved_engine_state) {
            Some(_) => Ok(()),
            None => Err(EngineBindingError::Invalid(
                "vLLM binding has invalid resolved engine state".to_owned(),
            )),
        }
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::VllmTransportFactory)
    }

    fn startup_fact_extractor(&self) -> Option<&dyn StartupFactExtractor> {
        Some(&startup_facts::VllmStartupFactExtractor)
    }

    fn startup_refusal(&self, output: &str) -> Option<StartupRefusal> {
        startup_refusal::startup_refusal(output)
    }
}

/// Returns the readiness policy mode retained in a current-semantics vLLM engine state.
fn readiness_mode(state: &ResolvedEngineModelState) -> Option<i32> {
    if state.engine_kind != EngineKind::Vllm as i32
        || state.semantics_version != realization::SEMANTICS_VERSION
    {
        return None;
    }
    match state.state.as_ref()? {
        State::Vllm(state) => state
            .fixed_settings
            .as_ref()?
            .readiness_policy
            .as_ref()
            .map(|policy| policy.mode),
        State::LlamaCpp(_) | State::Ninfer(_) => None,
    }
}
