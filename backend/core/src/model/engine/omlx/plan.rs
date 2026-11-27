//! Translation and validation of the portable oMLX launch plan for a resolved deployment.
//!
//! From a resolved deployment snapshot it builds the canonical command line of oMLX's package
//! launcher: the model snapshot, the deployment's model identity as the served id, the service
//! port, the context per request and parallel requests oMLX enforces, the whole-process memory
//! ceiling derived from the deployment's estimate, and the in-memory prompt cache size. The
//! launcher takes no other arguments, so custom arguments are refused. It also checks that a
//! retained plan exactly matches the plan its resolved state would produce.

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
};
use dotcore_proto::{
    dotcore::{
        catalog::v1::parameter_value::Value as EngineParameterValueKind,
        engine::{
            omlx::v1::OmlxResolvedModelState,
            v1::{EngineKind, resolved_engine_model_state::State},
        },
        model::v1::{
            PortableModelArtifactBinding, PortableModelLaunchArgumentToken,
            PortableModelLaunchPlan, ResolvedLocalModelDeploymentSnapshot,
            portable_model_artifact_binding::Artifact as BoundArtifact,
            portable_model_launch_argument_token::Value as ArgumentValue,
        },
    },
    google::protobuf::Empty,
};

/// Version of the symbolic plan; plans retained in another format must be recreated.
pub(super) const PLAN_FORMAT_VERSION: u32 = 1;
pub(super) const MODEL_BINDING_ID: &str = "model";
/// Name of the HTTP service the plan binds through its port token.
pub(super) const SERVICE_NAME: &str = "http";

/// oMLX's peak whole-process footprint over its memory guard ceiling under full load, as a
/// fraction in hundredths: it measured 1.07 at the pinned release. The ceiling is the estimate
/// divided by it, so the peak lands on the estimate. Re-measured on every engine bump.
const MEMORY_OVERSHOOT_HUNDREDTHS: u128 = 107;

/// Decimal places of the memory ceiling, rounded down so the ceiling never exceeds its share of
/// the estimate.
const CEILING_DECIMALS: u32 = 3;

const BYTES_PER_GIB: u128 = 1 << 30;
const MIB_PER_GIB: u64 = 1024;

/// Produces the single portable desired plan for an accepted oMLX selection.
pub(crate) fn translate(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<PortableModelLaunchPlan, OmlxPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(OmlxPlanError::NoFixedSettings)?;
    let model = snapshot
        .model_artifact
        .as_ref()
        .ok_or(OmlxPlanError::NoModelArtifact)?;
    // The launcher parses with argparse, where any token could name or abbreviate an option the
    // plan sets, and it takes no other arguments.
    if let Some(token) = snapshot.custom_argument_tokens.first() {
        return Err(OmlxPlanError::CustomArgument(
            crate::model::engine::custom_argument_flag(token).to_owned(),
        ));
    }
    let context_per_request = positive_parameter(snapshot, CONTEXT_PER_REQUEST_PARAMETER_ID)?;
    let parallel_requests = positive_parameter(snapshot, PARALLEL_REQUESTS_PARAMETER_ID)?;
    let prompt_cache_mib = unsigned_parameter(snapshot, &fixed.prompt_cache_mib_parameter_id)?;

    let mut arguments = vec![
        token(ArgumentValue::EngineExecutable(Empty {})),
        literal("--snapshot"),
        token(ArgumentValue::ArtifactBindingId(
            MODEL_BINDING_ID.to_owned(),
        )),
        literal("--served-id"),
        literal(&snapshot.model_id),
        literal("--port"),
        token(ArgumentValue::ServicePort(SERVICE_NAME.to_owned())),
    ];
    arguments.extend(literals([
        "--context-per-request",
        &context_per_request.to_string(),
        "--parallel-requests",
        &parallel_requests.to_string(),
        "--memory-ceiling-gb",
        &memory_ceiling_gib(snapshot)?,
        "--prompt-cache-gb",
        &mib_as_gib(prompt_cache_mib),
    ]));
    Ok(PortableModelLaunchPlan {
        arguments,
        format_version: PLAN_FORMAT_VERSION,
        artifact_bindings: vec![PortableModelArtifactBinding {
            binding_id: MODEL_BINDING_ID.to_owned(),
            artifact: Some(BoundArtifact::ModelArtifactId(model.artifact_id.clone())),
            read_only: true,
        }],
    })
}

/// Validates that a retained plan is canonical for its versioned resolved state.
pub(crate) fn validate_snapshot(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), OmlxPlanError> {
    if let Some(plan) = snapshot.desired_launch_plan.as_ref() {
        let found = plan.format_version;
        if found < PLAN_FORMAT_VERSION {
            return Err(OmlxPlanError::RetainedPlanFormat { found });
        }
        if found > PLAN_FORMAT_VERSION {
            return Err(OmlxPlanError::NewerRetainedPlanFormat { found });
        }
    }
    if !snapshot.resolved_options.is_empty() || !snapshot.auxiliary_artifacts.is_empty() {
        return Err(OmlxPlanError::InvalidResolvedState);
    }
    let expected = translate(snapshot)?;
    if snapshot.desired_launch_plan.as_ref() != Some(&expected) {
        return Err(OmlxPlanError::NonCanonicalPlan);
    }
    Ok(())
}

fn resolved_state(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<&OmlxResolvedModelState, OmlxPlanError> {
    snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            state.engine_kind == EngineKind::Omlx as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::Omlx(state) => Some(state),
            State::LlamaCpp(_) | State::Vllm(_) | State::Ninfer(_) | State::Strata(_) => None,
        })
        .ok_or(OmlxPlanError::InvalidResolvedState)
}

/// Renders the whole-process memory ceiling in GiB: the deployment's accelerator and host
/// estimates together, since both draw from the unified memory oMLX's guard measures, divided by
/// oMLX's overshoot and rounded down to [`CEILING_DECIMALS`] places.
fn memory_ceiling_gib(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<String, OmlxPlanError> {
    let sizing = snapshot.sizing.as_ref();
    let accelerator = sizing
        .and_then(|sizing| sizing.accelerator_memory_bytes)
        .ok_or(OmlxPlanError::SizingUnavailable)?;
    let host = sizing
        .and_then(|sizing| sizing.host_memory_bytes)
        .unwrap_or_default();
    let scale = 10_u128.pow(CEILING_DECIMALS);
    let scaled = (u128::from(accelerator) + u128::from(host)) * 100 * scale
        / (MEMORY_OVERSHOOT_HUNDREDTHS * BYTES_PER_GIB);
    if scaled == 0 {
        return Err(OmlxPlanError::EstimateBelowCeilingPrecision);
    }
    Ok(format!(
        "{}.{:0width$}",
        scaled / scale,
        scaled % scale,
        width = CEILING_DECIMALS as usize
    ))
}

/// Renders a MiB size as the exact decimal GiB the launcher takes: one MiB is 1/1024 GiB, a
/// fraction with at most ten decimal places.
fn mib_as_gib(mib: u64) -> String {
    let whole = mib / MIB_PER_GIB;
    let remainder = mib % MIB_PER_GIB;
    if remainder == 0 {
        return whole.to_string();
    }
    // 1/1024 = 0.0009765625, so the remainder in ten-billionths is exact.
    let fraction = format!("{:010}", remainder * 9_765_625);
    format!("{whole}.{}", fraction.trim_end_matches('0'))
}

fn positive_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, OmlxPlanError> {
    unsigned_parameter(snapshot, parameter_id).and_then(|value| {
        (value > 0)
            .then_some(value)
            .ok_or_else(|| OmlxPlanError::ParameterNotConfigured(parameter_id.to_owned()))
    })
}

fn unsigned_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, OmlxPlanError> {
    snapshot
        .engine_parameter_values
        .as_ref()
        .and_then(|values| {
            values
                .values
                .iter()
                .find(|value| value.parameter_id == parameter_id)
        })
        .and_then(|value| match value.value.as_ref() {
            Some(EngineParameterValueKind::UnsignedInteger(value)) => Some(*value),
            _ => None,
        })
        .ok_or_else(|| OmlxPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn literals<const N: usize>(values: [&str; N]) -> Vec<PortableModelLaunchArgumentToken> {
    values.into_iter().map(literal).collect()
}

fn literal(value: &str) -> PortableModelLaunchArgumentToken {
    token(ArgumentValue::Literal(value.to_owned()))
}

fn token(value: ArgumentValue) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken { value: Some(value) }
}

/// Indicates that resolved values cannot produce a complete portable oMLX plan.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OmlxPlanError {
    #[error("oMLX launch plan has invalid resolved engine state")]
    InvalidResolvedState,

    #[error("oMLX launch plan is not canonical for its resolved state")]
    NonCanonicalPlan,

    #[error(
        "oMLX deployment was created with launch plan format {found}, which this Dotcore version \
         no longer supports; delete and recreate the deployment"
    )]
    RetainedPlanFormat { found: u32 },

    #[error(
        "oMLX deployment was created with launch plan format {found} by a newer Dotcore version; \
         upgrade Dotcore, or delete and recreate the deployment with this version"
    )]
    NewerRetainedPlanFormat { found: u32 },

    #[error("oMLX launch plan has no fixed settings")]
    NoFixedSettings,

    #[error("oMLX launch plan has no model artifact")]
    NoModelArtifact,

    #[error("oMLX launch plan has no valid value for engine parameter {0:?}")]
    ParameterNotConfigured(String),

    #[error(
        "oMLX launch plan has no accelerator memory footprint; the catalog binding must declare \
         an acceleratorMemory estimate"
    )]
    SizingUnavailable,

    #[error("oMLX launch plan has a memory estimate too small to set a memory ceiling")]
    EstimateBelowCeilingPrecision,

    #[error(
        "oMLX takes no custom arguments, but got {0:?}; its package launcher accepts only the \
         options Dotcore sets"
    )]
    CustomArgument(String),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::{
        ModelDeploymentSnapshot, ResolvedModelOption, ResolvedSizing, model_deployment_snapshot,
    };

    use super::*;
    use crate::{
        model::engine::definition::EngineDefinition, test_support::omlx::local_model_snapshot,
    };

    #[test]
    fn the_profile_launches_the_package_launcher_with_its_limits_and_memory_ceiling() {
        let snapshot = local(local_model_snapshot("qwen3-5-4b"));

        let plan = translate(&snapshot).expect("oMLX plan");

        assert_eq!(
            plan.arguments
                .iter()
                .map(|argument| match argument.value.as_ref().expect("token") {
                    ArgumentValue::EngineExecutable(_) => "{executable}".to_owned(),
                    ArgumentValue::ArtifactBindingId(id) => format!("{{artifact:{id}}}"),
                    ArgumentValue::ServicePort(name) => format!("{{port:{name}}}"),
                    ArgumentValue::Literal(value) => value.clone(),
                    other => panic!("unexpected token {other:?}"),
                })
                .collect::<Vec<_>>(),
            [
                "{executable}",
                "--snapshot",
                "{artifact:model}",
                "--served-id",
                "qwen3-5-4b",
                "--port",
                "{port:http}",
                "--context-per-request",
                "32768",
                "--parallel-requests",
                "2",
                // (10,000,000,000 + 2 GiB) / 1.07 = 10.5731 GiB, rounded down.
                "--memory-ceiling-gb",
                "10.573",
                "--prompt-cache-gb",
                "2",
            ]
        );
        assert_eq!(
            plan.artifact_bindings,
            [PortableModelArtifactBinding {
                binding_id: "model".to_owned(),
                artifact: Some(BoundArtifact::ModelArtifactId(
                    "fixture-mlx-4bit".to_owned()
                )),
                read_only: true,
            }]
        );
        assert_eq!(plan.format_version, 1);
    }

    #[test]
    fn the_memory_ceiling_divides_the_whole_estimate_by_the_overshoot() {
        let mut snapshot = local(local_model_snapshot("qwen3-5-4b"));
        let mut ceiling = |accelerator, host| {
            snapshot.sizing = Some(ResolvedSizing {
                accelerator_memory_bytes: accelerator,
                host_memory_bytes: host,
                ..Default::default()
            });
            memory_ceiling_gib(&snapshot).map_err(|error| error.to_string())
        };

        // 1.07 GiB of estimate is exactly a 1 GiB ceiling.
        assert_eq!(
            ceiling(Some(1_148_903_751), Some(0)),
            Ok("0.999".to_owned())
        );
        assert_eq!(ceiling(Some(1_148_903_752), None), Ok("1.000".to_owned()));
        assert_eq!(
            ceiling(Some(16 * 1024 * 1024 * 1024), Some(4 * 1024 * 1024 * 1024)),
            Ok("18.691".to_owned())
        );
        assert_eq!(
            ceiling(Some(1_000), Some(1_000)),
            Err(
                "oMLX launch plan has a memory estimate too small to set a memory ceiling"
                    .to_owned()
            )
        );
        assert_eq!(
            ceiling(None, Some(1_000_000_000)),
            Err(
                "oMLX launch plan has no accelerator memory footprint; the catalog binding must \
                 declare an acceleratorMemory estimate"
                    .to_owned()
            )
        );
    }

    #[test]
    fn the_prompt_cache_reaches_the_launcher_as_exact_gib() {
        for (mib, gib) in [
            (0, "0"),
            (1, "0.0009765625"),
            (100, "0.09765625"),
            (1_536, "1.5"),
            (2_048, "2"),
            (40_960, "40"),
        ] {
            assert_eq!(mib_as_gib(mib), gib, "{mib}");
        }
    }

    #[test]
    fn custom_arguments_are_refused() {
        let mut snapshot = local(local_model_snapshot("qwen3-5-4b"));
        snapshot.custom_argument_tokens = vec!["--max-tokens=8".to_owned()];

        assert_eq!(
            super::super::Omlx
                .translate(&snapshot)
                .expect_err("custom argument")
                .to_string(),
            "oMLX takes no custom arguments, but got \"--max-tokens\""
        );
    }

    #[test]
    fn a_retained_plan_must_be_canonical_for_its_resolved_state() {
        let snapshot = local(local_model_snapshot("qwen3-5-4b"));
        validate_snapshot(&snapshot).expect("canonical plan");

        let mut edited = snapshot.clone();
        edited
            .desired_launch_plan
            .as_mut()
            .expect("plan")
            .arguments
            .pop();
        let mut older = snapshot.clone();
        older
            .desired_launch_plan
            .as_mut()
            .expect("plan")
            .format_version = 0;
        let mut newer = snapshot.clone();
        newer
            .desired_launch_plan
            .as_mut()
            .expect("plan")
            .format_version = 2;
        let mut options = snapshot.clone();
        options.resolved_options = vec![ResolvedModelOption::default()];
        let mut resized = snapshot;
        resized
            .sizing
            .as_mut()
            .expect("sizing")
            .accelerator_memory_bytes = Some(20_000_000_000);

        for (case, snapshot, message) in [
            (
                "edited",
                edited,
                "oMLX launch plan is not canonical for its resolved state",
            ),
            (
                "older",
                older,
                "oMLX deployment was created with launch plan format 0, which this Dotcore version \
                 no longer supports; delete and recreate the deployment",
            ),
            (
                "newer",
                newer,
                "oMLX deployment was created with launch plan format 2 by a newer Dotcore version; \
                 upgrade Dotcore, or delete and recreate the deployment with this version",
            ),
            (
                "options",
                options,
                "oMLX launch plan has invalid resolved engine state",
            ),
            (
                "resized",
                resized,
                "oMLX launch plan is not canonical for its resolved state",
            ),
        ] {
            assert_eq!(
                validate_snapshot(&snapshot).expect_err(case).to_string(),
                message,
                "{case}"
            );
        }
    }

    #[test]
    fn a_zero_context_per_request_has_no_plan() {
        let mut snapshot = local(local_model_snapshot("qwen3-5-4b"));
        snapshot
            .engine_parameter_values
            .as_mut()
            .expect("values")
            .values
            .iter_mut()
            .find(|value| value.parameter_id == CONTEXT_PER_REQUEST_PARAMETER_ID)
            .expect("context per request")
            .value = Some(EngineParameterValueKind::UnsignedInteger(0));

        assert_eq!(
            translate(&snapshot).expect_err("no context").to_string(),
            "oMLX launch plan has no valid value for engine parameter \"context-per-request\""
        );
    }

    fn local(snapshot: ModelDeploymentSnapshot) -> ResolvedLocalModelDeploymentSnapshot {
        match snapshot.source.expect("snapshot source") {
            model_deployment_snapshot::Source::Local(local) => local,
            _ => panic!("local snapshot"),
        }
    }
}
