//! The ninfer engine: Catalog declaration validation, launch-plan translation, OCI workload
//! materialization, loopback readiness probing, and loopback inference binding.
//!
//! ninfer serves one pre-converted `.ninfer` weights file on one RTX 5090 through an
//! OpenAI-compatible HTTP server. Callers reach it only through its engine definition.
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
#[cfg(test)]
pub(crate) use plan::translate;
#[cfg(test)]
pub(crate) use realization::realize_model;

/// The ninfer engine.
pub(crate) struct Ninfer;

/// Inference request kinds ninfer serves: chat only.
const REQUEST_KINDS: &[InferenceRequestKind] = &[InferenceRequestKind::Chat];

impl EngineDefinition for Ninfer {
    fn kind(&self) -> EngineKind {
        EngineKind::Ninfer
    }

    fn name(&self) -> &'static str {
        "ninfer"
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

    fn validate_parameter_values(
        &self,
        _model: &Model,
        _configuration: &ModelEngineConfiguration,
        values: &EngineParameterValues,
    ) -> anyhow::Result<()> {
        declaration::validate_parameter_values(values)
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
        _resolved_engine_state: &ResolvedEngineModelState,
    ) -> Arc<dyn EngineReadinessProbe> {
        Arc::new(readiness::NinferReadinessProbe::new(
            endpoint,
            served_model_id,
        ))
    }

    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory> {
        Arc::new(binding::NinferTransportFactory)
    }
}

/// Largest MTP speculative-token count ninfer accepts.
const MAXIMUM_MTP_TOKEN_COUNT: u64 = 5;
