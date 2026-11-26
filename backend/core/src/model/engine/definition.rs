//! The behavior every supported inference engine provides, and the single list of those engines.
//!
//! Each engine package (llama.cpp, vLLM, ninfer, Strata) describes itself once: how its catalog
//! declarations are validated, how options become a launch plan and container workload, how
//! readiness is probed, and which transport reaches it. Catalog validation, deployment
//! realization, and the engine integration registry all look engines up here by kind, so adding
//! an engine means adding its package and one entry to the list.

use std::{collections::BTreeSet, sync::Arc};

use dotcore_artifact::PreparedModelArtifacts;
use dotcore_catalog::shared_parameters;
use dotcore_domain::WorkloadArtifactKind;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, EngineRelease, HardwareClass, InferenceRequestKind, Model,
        ModelArtifactBinding, ModelEngineConfiguration, ModelOptionValues, ParameterValues,
        UnsignedIntegerParameterSchema, parameter_definition, parameter_value,
    },
    engine::v1::{EngineKind, ResolvedEngineModelState},
    model::v1::{PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot},
};
use dotcore_runtime::{ExecutionTarget, LoopbackEndpoint, WorkloadSpec};

use super::{
    llama_cpp, ninfer, realization::ResolvedEngineSelection, startup_facts::StartupFactExtractor,
    startup_refusal::StartupRefusal, strata, vllm,
};
use crate::{
    inference::{EngineBindingError, EngineReadinessProbe, EngineTransportFactory},
    model::resolved::ResolvedModelLaunch,
};

/// Every supported engine, keyed by its unique engine kind.
pub(crate) static ENGINES: [&dyn EngineDefinition; 4] = [
    &llama_cpp::LlamaCpp,
    &vllm::Vllm,
    &ninfer::Ninfer,
    &strata::Strata,
];

/// Stateless, engine-owned behavior selected by engine kind.
pub(crate) trait EngineDefinition: Send + Sync {
    fn kind(&self) -> EngineKind;

    /// Engine name used in diagnostics, for example `llama.cpp`.
    fn name(&self) -> &'static str;

    /// Inference request kinds the engine serves; catalog configurations of this engine and its
    /// loopback bindings accept only these.
    fn request_kinds(&self) -> &'static [InferenceRequestKind];

    fn validate_engine_release(&self, engine: &EngineRelease) -> Vec<anyhow::Error>;

    fn validate_model_configuration(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        engine: &EngineRelease,
    ) -> Vec<anyhow::Error>;

    /// Checks the accelerator vendors the configuration's supported hardware classes select
    /// against the vendors the engine runs on.
    fn validate_hardware_support(
        &self,
        configuration: &ModelEngineConfiguration,
        hardware_classes: &[&HardwareClass],
    ) -> Vec<anyhow::Error>;

    /// How the engine relates the context values of one configuration, which resolution derives
    /// the omitted ones by. By default it is the shared rule, for engines whose requests share one
    /// total context.
    fn context_rule(
        &self,
        _configuration: &ModelEngineConfiguration,
    ) -> &'static dyn shared_parameters::ContextRule {
        &shared_parameters::SharedContextRule
    }

    /// Rejects complete engine parameter values the engine cannot launch together, or cannot launch
    /// for the model's configuration, even though each value lies within its own range. Launch
    /// resolution applies it after sizing derivations.
    fn validate_parameter_values(
        &self,
        _model: &Model,
        _configuration: &ModelEngineConfiguration,
        _values: &ParameterValues,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Whether the engine's launch consumes catalog sizing, so every binding must declare an
    /// accelerator memory footprint estimate.
    fn launch_requires_sizing(&self) -> bool {
        false
    }

    /// Selects the concrete engine state and artifacts for a configuration's option values.
    fn realize_model(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        binding: &ModelArtifactBinding,
        option_values: &[ModelOptionValues],
    ) -> anyhow::Result<ResolvedEngineSelection>;

    /// Sets of auxiliary artifacts the configuration's options can add to one launch, one set per
    /// combination and the empty set for a launch that selects none. A launch realizes the binding's
    /// declared artifacts plus one of these sets, so a binding whose configuration offers several
    /// sets has several measurement series. Engines whose options add no auxiliary artifact answer
    /// with the single empty set.
    fn selectable_auxiliary_artifact_sets(
        &self,
        _configuration: &ModelEngineConfiguration,
    ) -> Vec<BTreeSet<String>> {
        vec![BTreeSet::new()]
    }

    fn validate_snapshot(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<()>;

    fn translate(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<PortableModelLaunchPlan>;

    /// Reports whether the snapshot places KV cache in host memory.
    fn enables_host_kv_offload(
        &self,
        snapshot: &ResolvedLocalModelDeploymentSnapshot,
    ) -> anyhow::Result<bool>;

    /// Executable artifact kinds this engine's workloads can launch from.
    fn artifact_kinds(&self) -> &'static [WorkloadArtifactKind];

    fn workload(
        &self,
        model: &ResolvedModelLaunch,
        target: &ExecutionTarget,
        artifacts: &PreparedModelArtifacts,
    ) -> anyhow::Result<WorkloadSpec>;

    /// Returns the probe that tells when the service is ready under `served_model_id`. An engine
    /// that reports the context it started with fails readiness when it is below the deployment's
    /// `context_per_request`, since it would serve less than the deployment resolved.
    fn readiness_probe(
        &self,
        endpoint: LoopbackEndpoint,
        served_model_id: String,
        resolved_engine_state: &ResolvedEngineModelState,
        context_per_request: Option<u64>,
    ) -> Arc<dyn EngineReadinessProbe>;

    /// Rejects resolved engine state this engine cannot bind a service with.
    fn validate_binding_state(
        &self,
        _resolved_engine_state: &ResolvedEngineModelState,
    ) -> Result<(), EngineBindingError> {
        Ok(())
    }

    /// Returns the transport that reaches this engine's service on a loopback port.
    fn transport_factory(&self) -> Arc<dyn EngineTransportFactory>;

    /// Returns the parser for engine-reported startup memory facts, when the engine has one.
    fn startup_fact_extractor(&self) -> Option<&dyn StartupFactExtractor> {
        None
    }

    /// Recognizes the engine's own reason for stopping during startup in its output, in
    /// chronological order with one entry per line. Returns none when no known reason is found.
    fn startup_refusal(&self, _output: &str) -> Option<StartupRefusal> {
        None
    }
}

/// A custom argument the operator supplied that the engine integration cannot accept: either it
/// sets the same option from the resolved configuration, or the engine takes no custom arguments.
/// It names only the flag, because the argument's value may be sensitive.
#[derive(Debug, thiserror::Error)]
pub struct CustomArgumentConflict {
    engine: &'static str,
    flag: String,
    engine_takes_none: bool,
}

impl CustomArgumentConflict {
    /// The argument collides with an option the engine integration sets itself.
    pub(crate) fn new(engine: &'static str, token: &str) -> Self {
        Self {
            engine,
            flag: custom_argument_flag(token).to_owned(),
            engine_takes_none: false,
        }
    }

    /// The engine accepts no custom arguments at all.
    pub(crate) fn unsupported(engine: &'static str, token: &str) -> Self {
        Self {
            engine_takes_none: true,
            ..Self::new(engine, token)
        }
    }
}

impl std::fmt::Display for CustomArgumentConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.engine_takes_none {
            write!(
                f,
                "{} takes no custom arguments, but got {:?}",
                self.engine, self.flag
            )
        } else {
            write!(
                f,
                "{} custom argument {:?} conflicts with resolved settings",
                self.engine, self.flag
            )
        }
    }
}

/// Returns the flag part of a custom argument token, without any `=value` suffix.
pub(crate) fn custom_argument_flag(token: &str) -> &str {
    token.split_once('=').map_or(token, |(flag, _)| flag)
}

/// Returns the engine declared with the given raw engine kind, if it is supported.
pub(crate) fn definition(kind: i32) -> Option<&'static dyn EngineDefinition> {
    ENGINES
        .iter()
        .copied()
        .find(|engine| engine.kind() as i32 == kind)
}

/// Returns the unsigned value of one engine parameter, or none when it is absent or not unsigned.
pub(crate) fn unsigned_parameter_value(
    values: &ParameterValues,
    parameter_id: &str,
) -> Option<u64> {
    values
        .values
        .iter()
        .find(|value| value.parameter_id == parameter_id)
        .and_then(|value| match value.value {
            Some(parameter_value::Value::UnsignedInteger(value)) => Some(value),
            _ => None,
        })
}

/// Returns the range an unsigned engine parameter of the schema accepts, or none when the schema
/// doesn't declare it as unsigned.
pub(crate) fn unsigned_parameter_range<'a>(
    schema: &'a EngineParameterSchema,
    parameter_id: &str,
) -> Option<&'a UnsignedIntegerParameterSchema> {
    schema
        .parameters
        .iter()
        .find(|definition| definition.parameter_id == parameter_id)
        .and_then(|definition| match definition.parameter_type.as_ref() {
            Some(parameter_definition::ParameterType::UnsignedInteger(range)) => Some(range),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    #[test]
    fn engine_kinds_are_unique_and_specified() {
        let kinds = ENGINES
            .iter()
            .map(|engine| engine.kind())
            .collect::<BTreeSet<_>>();

        assert_eq!(kinds.len(), ENGINES.len());
        assert!(!kinds.contains(&EngineKind::Unspecified));
    }

    #[test]
    fn definition_selects_each_engine_and_rejects_unknown_kinds() {
        for kind in [
            EngineKind::LlamaCpp,
            EngineKind::Vllm,
            EngineKind::Ninfer,
            EngineKind::Strata,
        ] {
            assert_eq!(
                definition(kind as i32).map(|engine| engine.kind()),
                Some(kind)
            );
        }
        assert!(definition(EngineKind::Unspecified as i32).is_none());
        assert!(definition(i32::MAX).is_none());
    }

    #[test]
    fn engines_declare_the_artifact_kinds_they_launch_from() {
        for engine in ENGINES {
            let expected: &[WorkloadArtifactKind] = match engine.kind() {
                EngineKind::LlamaCpp => &[WorkloadArtifactKind::Oci, WorkloadArtifactKind::Nix],
                _ => &[WorkloadArtifactKind::Oci],
            };
            assert_eq!(engine.artifact_kinds(), expected, "{}", engine.name());
        }
    }
}
