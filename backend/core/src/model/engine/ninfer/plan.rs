//! Translation and validation of the portable ninfer launch plan for a resolved deployment.
//!
//! From a resolved deployment snapshot it builds the canonical `ninfer-serve` command line: the
//! `.ninfer` weights mounted read-only, the deployment's model identity as the served alias, one
//! container-visible GPU, the fixed settings, the engine parameter values, and the selected
//! options, followed by any custom arguments that do not touch adapter-owned flags. It also checks
//! that a retained plan exactly matches the plan its resolved state would produce.

use std::collections::BTreeMap;

use dotcore_catalog::{
    options::{SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, boolean_value, unsigned_value},
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID,
    },
};
use dotcore_proto::dotcore::{
    catalog::v1::{ModelOptionValues, parameter_value::Value as EngineParameterValueKind},
    engine::{
        ninfer::v1::{
            NinferFixedModelSettings, NinferKvCacheDataType, NinferResolvedModelState,
            ninfer_option_realization::Realization,
        },
        v1::{EngineKind, resolved_engine_model_state::State},
    },
    model::v1::{
        PortableModelArtifactBinding, PortableModelLaunchArgumentToken, PortableModelLaunchPlan,
        ResolvedLocalModelDeploymentSnapshot,
        portable_model_artifact_binding::Artifact as BoundArtifact,
        portable_model_launch_argument_token::Value as ArgumentValue,
    },
};

pub(super) const PLAN_FORMAT_VERSION: u32 = 5;
const SERVER_BINARY: &str = "/usr/local/bin/ninfer-serve";
pub(super) const MODEL_BINDING_ID: &str = "model";
/// Tokens in one page of ninfer's paged KV history, an engine constant.
pub(super) const KV_PAGE_TOKENS: u64 = 64;
/// ninfer matches flags exactly and a repeated flag overrides the earlier one, so custom arguments
/// must not repeat any flag the plan owns.
const ADAPTER_OWNED_FLAGS: [&str; 23] = [
    "--model-id",
    "--host",
    "--port",
    "--device",
    "--vision",
    "--no-vision",
    "--max-context",
    "--context-capacity",
    "--spec",
    "--draft-tokens",
    "--max-concurrency",
    "--pending-timeout",
    "--kv-dtype",
    "--host-cache-capacity",
    "--proposal-head",
    "--preserve-thinking",
    "--no-preserve-thinking",
    "--memory-headroom",
    // A configuration file could silently replace any setting the plan owns, or split the
    // context pool across distributed ranks.
    "--config",
    // Root-only mode would silently change the cross-request history the plan relies on, and the
    // context pool's startup minimum counts the handover state that prefix reuse needs.
    "--prefix-reuse",
    "--no-prefix-reuse",
    // The context pool's startup minimum assumes the default prefill chunk.
    "--prefill-chunk",
    // Authentication on every route but `/health` would fail readiness and routed requests.
    "--api-key",
];

/// Produces the single portable desired plan for an accepted ninfer selection.
pub(crate) fn translate(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<PortableModelLaunchPlan, NinferPlanError> {
    let state = resolved_state(snapshot)?;
    let fixed = state
        .fixed_settings
        .as_ref()
        .ok_or(NinferPlanError::NoFixedSettings)?;
    let model = snapshot
        .model_artifact
        .as_ref()
        .ok_or(NinferPlanError::NoModelArtifact)?;
    let options = selected_options(snapshot, state)?;
    let kv_dtype = match NinferKvCacheDataType::try_from(fixed.kv_cache_data_type) {
        Ok(NinferKvCacheDataType::Fp8) => "fp8",
        Ok(NinferKvCacheDataType::Unspecified) | Err(_) => {
            return Err(NinferPlanError::InvalidResolvedState);
        }
    };
    if fixed.pending_timeout_milliseconds == 0 {
        return Err(NinferPlanError::InvalidResolvedState);
    }
    let context_per_request =
        positive_parameter(snapshot, CONTEXT_PER_REQUEST_PARAMETER_ID)?.to_string();
    let total_context = positive_parameter(snapshot, TOTAL_CONTEXT_PARAMETER_ID)?;
    let parallel_requests = positive_parameter(snapshot, PARALLEL_REQUESTS_PARAMETER_ID)?;
    let context_capacity = context_capacity_bytes(
        fixed,
        total_context,
        parallel_requests,
        options.mtp.is_some(),
    )?;

    let mut arguments = vec![literal(SERVER_BINARY), binding(MODEL_BINDING_ID)];
    // The runtime exposes exactly one GPU to the container, so it is always device 0.
    arguments.extend(literals([
        "--model-id",
        &snapshot.model_id,
        "--host",
        "0.0.0.0",
        "--port",
        "8080",
        "--device",
        "0",
    ]));
    if options.vision {
        arguments.push(literal("--vision"));
    }
    arguments.extend(literals([
        "--max-context",
        &context_per_request,
        "--context-capacity",
        &context_capacity.to_string(),
    ]));
    if let Some(mtp) = options.mtp.as_ref() {
        arguments.extend(literals([
            "--spec",
            "mtp",
            "--draft-tokens",
            &mtp.draft_tokens.to_string(),
        ]));
    }
    arguments.extend(literals([
        "--max-concurrency",
        &parallel_requests.to_string(),
        "--pending-timeout",
        &format!("{}ms", fixed.pending_timeout_milliseconds),
        "--kv-dtype",
        kv_dtype,
        "--host-cache-capacity",
        &format!(
            "{}MiB",
            parameter(snapshot, &fixed.host_context_mib_parameter_id)?
        ),
        // ninfer holds a startup headroom out of the memory it plans against, even for an explicit
        // context pool. Dotcore's sizing estimate and fit already keep the memory margin, so a
        // second one would refuse context that fits the card.
        "--memory-headroom",
        "0B",
    ]));
    if options
        .mtp
        .as_ref()
        .is_some_and(|mtp| mtp.optimized_proposal_head)
    {
        arguments.extend(literals(["--proposal-head", "optimized"]));
    }
    if fixed.preserve_thinking {
        arguments.push(literal("--preserve-thinking"));
    }

    validate_custom_argument_tokens(&snapshot.custom_argument_tokens)?;
    arguments.extend(
        snapshot
            .custom_argument_tokens
            .iter()
            .map(|token| literal(token)),
    );
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

/// Reports whether the resolved snapshot pins host memory for context storage. A zero budget disables it.
pub(crate) fn enables_host_kv_offload(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<bool, NinferPlanError> {
    let fixed = resolved_state(snapshot)?
        .fixed_settings
        .as_ref()
        .ok_or(NinferPlanError::NoFixedSettings)?;
    Ok(parameter(snapshot, &fixed.host_context_mib_parameter_id)? > 0)
}

/// Validates that a retained plan is canonical for its versioned resolved state.
pub(crate) fn validate_snapshot(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), NinferPlanError> {
    if let Some(plan) = snapshot.desired_launch_plan.as_ref() {
        let found = plan.format_version;
        if found < PLAN_FORMAT_VERSION {
            return Err(NinferPlanError::RetainedPlanFormat { found });
        }
        if found > PLAN_FORMAT_VERSION {
            return Err(NinferPlanError::NewerRetainedPlanFormat { found });
        }
    }
    validate_resolved_consistency(snapshot)?;
    let expected = translate(snapshot)?;
    if snapshot.desired_launch_plan.as_ref() != Some(&expected) {
        return Err(NinferPlanError::NonCanonicalPlan);
    }
    Ok(())
}

fn resolved_state(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<&NinferResolvedModelState, NinferPlanError> {
    snapshot
        .resolved_engine_state
        .as_ref()
        .filter(|state| {
            state.engine_kind == EngineKind::Ninfer as i32
                && state.semantics_version == super::realization::SEMANTICS_VERSION
        })
        .and_then(|state| state.state.as_ref())
        .and_then(|state| match state {
            State::Ninfer(state) => Some(state),
            State::LlamaCpp(_) | State::Vllm(_) | State::Strata(_) => None,
        })
        .ok_or(NinferPlanError::InvalidResolvedState)
}

fn validate_resolved_consistency(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
) -> Result<(), NinferPlanError> {
    let state = resolved_state(snapshot)?;
    let options = selected_options(snapshot, state)?;
    let image_capable = snapshot
        .input_capabilities
        .as_ref()
        .and_then(|capabilities| capabilities.image_input_capacity.as_ref())
        .is_some();
    if options.vision != image_capable || !snapshot.auxiliary_artifacts.is_empty() {
        return Err(NinferPlanError::InvalidResolvedState);
    }
    Ok(())
}

/// Option effects selected by the resolved option values.
struct SelectedOptions {
    vision: bool,
    mtp: Option<SelectedMtp>,
}

struct SelectedMtp {
    draft_tokens: u64,
    optimized_proposal_head: bool,
}

fn selected_options(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    state: &NinferResolvedModelState,
) -> Result<SelectedOptions, NinferPlanError> {
    let values = snapshot
        .resolved_options
        .iter()
        .filter_map(|option| option.values.as_ref())
        .map(|values| (values.option_id.as_str(), values))
        .collect::<BTreeMap<_, &ModelOptionValues>>();
    if values.len() != snapshot.resolved_options.len()
        || values.len() != state.option_realizations.len()
    {
        return Err(NinferPlanError::InvalidResolvedState);
    }

    let mut selected = SelectedOptions {
        vision: false,
        mtp: None,
    };
    for realization in &state.option_realizations {
        let values = values
            .get(realization.option_id.as_str())
            .ok_or(NinferPlanError::InvalidResolvedState)?;
        match realization.realization.as_ref() {
            Some(Realization::Vision(vision)) => {
                selected.vision = boolean_value(values, &vision.enabled_parameter_id)
                    .ok_or(NinferPlanError::InvalidResolvedState)?;
            }
            Some(Realization::Mtp(mtp)) => {
                let draft_tokens = unsigned_value(values, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
                    .ok_or(NinferPlanError::InvalidResolvedState)?;
                if draft_tokens > 0 {
                    selected.mtp = Some(SelectedMtp {
                        draft_tokens,
                        optimized_proposal_head: mtp.optimized_proposal_head,
                    });
                }
            }
            None => return Err(NinferPlanError::InvalidResolvedState),
        }
    }
    Ok(selected)
}

/// Bytes of device context pool that hold a total context of history shared by the parallel
/// requests, with each request's continuation state and, with MTP, its draft history.
///
/// ninfer splits the pool into blocks the size of one request state, each holding records of one
/// kind, and uses only whole blocks. The result counts whole blocks for every record kind, so the
/// engine gets the full total context, every request's state and its startup minimum.
fn context_capacity_bytes(
    fixed: &NinferFixedModelSettings,
    total_context: u64,
    parallel_requests: u64,
    mtp: bool,
) -> Result<u64, NinferPlanError> {
    let block = fixed.state_image_bytes;
    let speculative = u64::from(mtp);
    // A speculative request holds a second state and one more page for its pending draft.
    let request_records = parallel_requests
        .checked_mul(1 + speculative)
        .ok_or(NinferPlanError::InvalidResolvedState)?;
    // One more state covers the prefix handover the engine checks at startup with prefix reuse.
    let states = request_records + 1;
    // Each request rounds its history up to whole pages, so it adds at most one page.
    let pages = total_context.div_ceil(KV_PAGE_TOKENS) + request_records;
    let mut blocks = states + kv_blocks(pages, fixed.kv_bytes_per_token, block)?;
    if mtp {
        blocks += kv_blocks(pages, fixed.draft_kv_bytes_per_token, block)?;
    }
    blocks
        .checked_mul(block)
        .ok_or(NinferPlanError::InvalidResolvedState)
}

fn kv_blocks(pages: u64, bytes_per_token: u64, block: u64) -> Result<u64, NinferPlanError> {
    let pages_per_block = bytes_per_token
        .checked_mul(KV_PAGE_TOKENS)
        .filter(|page| *page > 0)
        .map(|page| block / page)
        .filter(|pages_per_block| *pages_per_block > 0)
        .ok_or(NinferPlanError::InvalidResolvedState)?;
    Ok(pages.div_ceil(pages_per_block))
}

fn positive_parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, NinferPlanError> {
    Some(parameter(snapshot, parameter_id)?)
        .filter(|value| *value > 0)
        .ok_or_else(|| NinferPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn parameter(
    snapshot: &ResolvedLocalModelDeploymentSnapshot,
    parameter_id: &str,
) -> Result<u64, NinferPlanError> {
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
        .ok_or_else(|| NinferPlanError::ParameterNotConfigured(parameter_id.to_owned()))
}

fn validate_custom_argument_tokens(tokens: &[String]) -> Result<(), NinferPlanError> {
    for token in tokens {
        let flag = token
            .split_once('=')
            .map_or(token.as_str(), |(flag, _)| flag);
        if ADAPTER_OWNED_FLAGS.contains(&flag) {
            return Err(NinferPlanError::ConflictingCustomArgument(token.clone()));
        }
    }
    Ok(())
}

fn literals<const N: usize>(values: [&str; N]) -> Vec<PortableModelLaunchArgumentToken> {
    values.into_iter().map(literal).collect()
}

fn literal(value: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(ArgumentValue::Literal(value.to_owned())),
    }
}

fn binding(binding_id: &str) -> PortableModelLaunchArgumentToken {
    PortableModelLaunchArgumentToken {
        value: Some(ArgumentValue::ArtifactBindingId(binding_id.to_owned())),
    }
}

/// Indicates that resolved values cannot produce a complete portable ninfer plan.
#[derive(Debug, thiserror::Error)]
pub(crate) enum NinferPlanError {
    #[error("ninfer launch plan has invalid resolved engine state")]
    InvalidResolvedState,

    #[error("ninfer launch plan is not canonical for its resolved state")]
    NonCanonicalPlan,

    #[error(
        "ninfer deployment was created with launch plan format {found}, which this Dotcore \
         version no longer supports; delete and recreate the deployment"
    )]
    RetainedPlanFormat { found: u32 },

    #[error(
        "ninfer deployment was created with launch plan format {found} by a newer Dotcore \
         version; upgrade Dotcore, or delete and recreate the deployment with this version"
    )]
    NewerRetainedPlanFormat { found: u32 },

    #[error("ninfer launch plan has no fixed settings")]
    NoFixedSettings,

    #[error("ninfer launch plan has no model artifact")]
    NoModelArtifact,

    #[error("ninfer launch plan has no valid value for engine parameter {0:?}")]
    ParameterNotConfigured(String),

    #[error("ninfer custom argument {0:?} conflicts with resolved settings")]
    ConflictingCustomArgument(String),
}

#[cfg(test)]
mod tests {
    use dotcore_proto::dotcore::model::v1::model_deployment_snapshot::Source;

    use super::*;
    use crate::test_support::ninfer::{
        local_model_snapshot, local_model_snapshot_with, parameter_values, refresh_launch_plan,
    };

    /// Argv of the Qwen3.8 27B ninfer profile at engine defaults with vision and MTP at 3 draft
    /// tokens: one 32,768-token total context shared by 4 parallel requests, each up to all of it.
    const ENGINE_DEFAULTS_MTP_ARGV: [&str; 32] = [
        "/usr/local/bin/ninfer-serve",
        "/opt/dotcore/models/model.ninfer",
        "--model-id",
        "qwen3-8-27b",
        "--host",
        "0.0.0.0",
        "--port",
        "8080",
        "--device",
        "0",
        "--vision",
        "--max-context",
        "32768",
        "--context-capacity",
        "2771177472",
        "--spec",
        "mtp",
        "--draft-tokens",
        "3",
        "--max-concurrency",
        "4",
        "--pending-timeout",
        "120000ms",
        "--kv-dtype",
        "fp8",
        "--host-cache-capacity",
        "20480MiB",
        "--memory-headroom",
        "0B",
        "--proposal-head",
        "optimized",
        "--preserve-thinking",
    ];

    #[test]
    fn engine_defaults_with_mtp_produce_the_exact_arguments() {
        let local = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));

        let plan = translate(&local).expect("plan");

        assert_eq!(argv(&plan), ENGINE_DEFAULTS_MTP_ARGV);
        assert_eq!(plan.format_version, 5);
        assert_eq!(
            plan.artifact_bindings,
            [PortableModelArtifactBinding {
                binding_id: "model".to_owned(),
                artifact: Some(BoundArtifact::ModelArtifactId(
                    "fixture-ninfer-artifact".to_owned()
                )),
                read_only: true,
            }]
        );
        validate_snapshot(&local).expect("canonical default plan");
    }

    #[test]
    fn vision_and_mtp_toggle_only_their_flags() {
        let text_only = local(
            "qwen3-8-27b",
            local_model_snapshot_with("qwen3-8-27b", false, 0),
        );

        let plan = translate(&text_only).expect("plan");

        assert_eq!(
            argv(&plan),
            [
                "/usr/local/bin/ninfer-serve",
                "/opt/dotcore/models/model.ninfer",
                "--model-id",
                "qwen3-8-27b",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--device",
                "0",
                "--max-context",
                "32768",
                "--context-capacity",
                "2001405952",
                "--max-concurrency",
                "4",
                "--pending-timeout",
                "120000ms",
                "--kv-dtype",
                "fp8",
                "--host-cache-capacity",
                "20480MiB",
                "--memory-headroom",
                "0B",
                "--preserve-thinking",
            ]
        );
        validate_snapshot(&text_only).expect("canonical text-only plan");

        let vision_only = local(
            "qwen3-8-27b",
            local_model_snapshot_with("qwen3-8-27b", true, 0),
        );
        let vision_argv = argv(&translate(&vision_only).expect("plan"));
        assert!(vision_argv.contains(&"--vision".to_owned()));
        assert!(!vision_argv.contains(&"--spec".to_owned()));
        assert!(!vision_argv.contains(&"--proposal-head".to_owned()));

        let mtp_five = local(
            "qwen3-8-27b",
            local_model_snapshot_with("qwen3-8-27b", false, 5),
        );
        let plan = translate(&mtp_five).expect("plan");
        assert_eq!(
            argv(&plan),
            [
                "/usr/local/bin/ninfer-serve",
                "/opt/dotcore/models/model.ninfer",
                "--model-id",
                "qwen3-8-27b",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--device",
                "0",
                "--max-context",
                "32768",
                "--context-capacity",
                "2771177472",
                "--spec",
                "mtp",
                "--draft-tokens",
                "5",
                "--max-concurrency",
                "4",
                "--pending-timeout",
                "120000ms",
                "--kv-dtype",
                "fp8",
                "--host-cache-capacity",
                "20480MiB",
                "--memory-headroom",
                "0B",
                "--proposal-head",
                "optimized",
                "--preserve-thinking",
            ]
        );
        validate_snapshot(&mtp_five).expect("canonical MTP plan");
    }

    #[test]
    fn parameter_overrides_and_custom_arguments_reach_the_argv() {
        let mut local = local("served-alias", local_model_snapshot("served-alias"));
        local.engine_parameter_values = Some(parameter_values([
            (TOTAL_CONTEXT_PARAMETER_ID, 262_144),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, 131_072),
            (PARALLEL_REQUESTS_PARAMETER_ID, 2),
            ("host-context-mib", 0),
        ]));
        local.custom_argument_tokens = vec![
            "--media-cache-mib=64".to_owned(),
            "  spaced $(value)  ".to_owned(),
        ];
        refresh_launch_plan(&mut local);

        let argv = argv(local.desired_launch_plan.as_ref().expect("plan"));

        assert_eq!(argv[3], "served-alias");
        assert_eq!(
            argv[11..],
            [
                "--max-context",
                "131072",
                "--context-capacity",
                "10160984064",
                "--spec",
                "mtp",
                "--draft-tokens",
                "3",
                "--max-concurrency",
                "2",
                "--pending-timeout",
                "120000ms",
                "--kv-dtype",
                "fp8",
                "--host-cache-capacity",
                "0MiB",
                "--memory-headroom",
                "0B",
                "--proposal-head",
                "optimized",
                "--preserve-thinking",
                "--media-cache-mib=64",
                "  spaced $(value)  ",
            ]
        );
        assert!(!enables_host_kv_offload(&local).expect("zero host context"));
        validate_snapshot(&local).expect("canonical overridden plan");
    }

    #[test]
    fn context_capacity_counts_whole_pool_blocks_for_every_record_kind() {
        let fixed = NinferFixedModelSettings {
            state_image_bytes: 153_954_304,
            kv_bytes_per_token: 33_024,
            draft_kv_bytes_per_token: 2_064,
            ..NinferFixedModelSettings::default()
        };

        // Shapes launched on an RTX 5090 at these sizes: total context, parallel requests, MTP,
        // then the expected pool blocks.
        for (total_context, parallel_requests, mtp, blocks) in [
            (32_768, 4, true, 18),
            (196_608, 4, true, 55),
            (131_072, 1, false, 31),
            (1024, 8, true, 19),
            (2048, 8, false, 10),
        ] {
            assert_eq!(
                context_capacity_bytes(&fixed, total_context, parallel_requests, mtp)
                    .expect("capacity"),
                blocks * 153_954_304,
                "{total_context} tokens, {parallel_requests} requests, MTP {mtp}"
            );
        }
    }

    #[test]
    fn context_capacity_rejects_pages_larger_than_a_block() {
        let fixed = NinferFixedModelSettings {
            state_image_bytes: 1024,
            kv_bytes_per_token: 33_024,
            draft_kv_bytes_per_token: 2_064,
            ..NinferFixedModelSettings::default()
        };

        assert!(matches!(
            context_capacity_bytes(&fixed, 32_768, 4, false),
            Err(NinferPlanError::InvalidResolvedState)
        ));
    }

    #[test]
    fn host_kv_offload_follows_the_host_context_parameter() {
        let local = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));

        assert!(enables_host_kv_offload(&local).expect("default host context"));
    }

    #[test]
    fn zero_shared_context_parameters_cannot_launch() {
        for parameter_id in [
            TOTAL_CONTEXT_PARAMETER_ID,
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            PARALLEL_REQUESTS_PARAMETER_ID,
        ] {
            let mut local = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
            local
                .engine_parameter_values
                .as_mut()
                .expect("values")
                .values
                .iter_mut()
                .find(|value| value.parameter_id == parameter_id)
                .expect("parameter")
                .value = Some(EngineParameterValueKind::UnsignedInteger(0));

            assert_eq!(
                translate(&local).expect_err("zero parameter").to_string(),
                format!(
                    "ninfer launch plan has no valid value for engine parameter {parameter_id:?}"
                )
            );
        }
    }

    #[test]
    fn custom_arguments_reject_adapter_owned_flags() {
        for token in [
            "--model-id=other",
            "--port",
            "--device=1",
            "--max-context",
            "--kv-dtype=bf16",
            "--spec=dflash2",
            "--context-capacity=auto",
            "--host-cache-capacity",
            "--memory-headroom=0B",
            "--prefill-chunk",
            "--config=serve.toml",
            "--prefix-reuse",
            "--no-prefix-reuse",
            "--no-vision",
            "--no-preserve-thinking",
            "--api-key",
            "--api-key=secret",
        ] {
            let mut local = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
            local.custom_argument_tokens = vec![token.to_owned()];

            assert_eq!(
                translate(&local).expect_err("owned flag").to_string(),
                format!("ninfer custom argument {token:?} conflicts with resolved settings")
            );
        }
    }

    #[test]
    fn snapshot_validation_rejects_non_canonical_plans_and_inconsistent_state() {
        let mut stale = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
        stale.model_id = "renamed".to_owned();
        assert!(matches!(
            validate_snapshot(&stale),
            Err(NinferPlanError::NonCanonicalPlan)
        ));

        let mut no_image = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
        no_image
            .input_capabilities
            .as_mut()
            .expect("capabilities")
            .image_input_capacity = None;
        assert!(matches!(
            validate_snapshot(&no_image),
            Err(NinferPlanError::InvalidResolvedState)
        ));

        let mut wrong_version = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
        wrong_version
            .resolved_engine_state
            .as_mut()
            .expect("state")
            .semantics_version += 1;
        assert!(matches!(
            validate_snapshot(&wrong_version),
            Err(NinferPlanError::InvalidResolvedState)
        ));
    }

    #[test]
    fn snapshot_validation_rejects_retained_plans_of_other_formats() {
        for (format_version, expected) in [
            (
                PLAN_FORMAT_VERSION - 1,
                "ninfer deployment was created with launch plan format 4, which this Dotcore \
                 version no longer supports; delete and recreate the deployment",
            ),
            (
                PLAN_FORMAT_VERSION + 1,
                "ninfer deployment was created with launch plan format 6 by a newer Dotcore \
                 version; upgrade Dotcore, or delete and recreate the deployment with this version",
            ),
        ] {
            let mut retained = local("qwen3-8-27b", local_model_snapshot("qwen3-8-27b"));
            retained
                .desired_launch_plan
                .as_mut()
                .expect("retained plan")
                .format_version = format_version;

            assert_eq!(
                validate_snapshot(&retained)
                    .expect_err("retained plan format")
                    .to_string(),
                expected
            );
        }
    }

    fn local(
        model_id: &str,
        snapshot: dotcore_proto::dotcore::model::v1::ModelDeploymentSnapshot,
    ) -> ResolvedLocalModelDeploymentSnapshot {
        match snapshot.source {
            Some(Source::Local(local)) if local.model_id == model_id => local,
            _ => panic!("local ninfer snapshot"),
        }
    }

    /// Renders the plan with the model binding at its container path.
    fn argv(plan: &PortableModelLaunchPlan) -> Vec<String> {
        plan.arguments
            .iter()
            .map(|argument| match argument.value.as_ref() {
                Some(ArgumentValue::Literal(value)) => value.clone(),
                Some(ArgumentValue::ArtifactBindingId(id)) if id == "model" => {
                    super::super::workload::CONTAINER_MODEL_PATH.to_owned()
                }
                other => panic!("unexpected argument {other:?}"),
            })
            .collect()
    }
}
