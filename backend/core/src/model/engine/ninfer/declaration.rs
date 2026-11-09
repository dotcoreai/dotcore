//! ninfer-specific validation of catalog engine releases and model engine configurations.
//!
//! A release must declare well-formed unsigned-integer parameters, the shared context parameters
//! within ninfer's limits (for example 1 to 8 parallel requests), and only linux/amd64 artifacts,
//! the one platform ninfer builds for. A configuration must bind single pre-converted `.ninfer`
//! weight files, run on exactly one NVIDIA accelerator, reference every other release parameter,
//! and realize every model option exactly once, with MTP speculative-token counts ninfer accepts.
//! Launch values must also give ninfer a total context its parallel requests can fill.

use std::collections::BTreeSet;

use dotcore_catalog::{
    options::SPECULATIVE_TOKEN_COUNT_PARAMETER_ID,
    shared_parameters::{
        CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
        TOTAL_CONTEXT_PARAMETER_ID, is_shared_parameter,
    },
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterValues, EngineRelease, HardwareClass, Model, ModelConfigurationOption,
        ModelEngineConfiguration, ModelInputModality, ModelParallelShape,
        SpeculativeDecodingMethod, engine_parameter_definition::ParameterType,
        hugging_face_artifact_source::Content as HuggingFaceContent,
        model_option::Kind as ModelOptionKind,
        model_option_parameter_definition::ParameterType as ModelOptionParameterType,
    },
    engine::{
        ninfer::v1::{
            NinferFixedModelSettings, NinferKvCacheDataType, NinferModelConfiguration,
            ninfer_option_realization::Realization,
        },
        v1::{EngineKind, engine_model_configuration::Configuration},
    },
};

use super::MAXIMUM_MTP_TOKEN_COUNT;
use crate::model::engine::{
    declaration::{classes_outside_vendors, kv_cache_precision_option_ids},
    definition::{unsigned_parameter_maximum, unsigned_parameter_value},
};

/// Largest parallel request count ninfer admits.
const MAXIMUM_PARALLEL_REQUESTS: u64 = 8;
/// Largest context one ninfer request can attend to, in tokens.
const MAXIMUM_CONTEXT_PER_REQUEST_TOKENS: u64 = 262_144;
/// Largest total context ninfer admits: every parallel request at its largest context.
const MAXIMUM_TOTAL_CONTEXT_TOKENS: u64 =
    MAXIMUM_PARALLEL_REQUESTS * MAXIMUM_CONTEXT_PER_REQUEST_TOKENS;
const MODEL_FILE_EXTENSION: &str = ".ninfer";

/// Rejects a total context ninfer refuses at startup: above what the parallel requests use at
/// their full context per request, so part of it could never hold a request.
pub(crate) fn validate_parameter_values(values: &EngineParameterValues) -> anyhow::Result<()> {
    let (Some(total_context), Some(context_per_request), Some(parallel_requests)) = (
        unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID),
        unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID),
        unsigned_parameter_value(values, PARALLEL_REQUESTS_PARAMETER_ID),
    ) else {
        return Ok(());
    };
    let maximum = parallel_requests.saturating_mul(context_per_request);
    if total_context > maximum {
        anyhow::bail!(
            "total context of {total_context} tokens is above the {maximum} tokens that {parallel_requests} parallel requests can use at this context per request; lower the total context, or raise the parallel requests or the context per request"
        );
    }
    Ok(())
}

/// Accepts a release of non-empty unsigned-integer parameters that declares the shared context
/// parameters within ninfer's limits and ships only linux/amd64 artifacts. The catalog checks the
/// shared parameters' fixed shape.
pub(crate) fn validate_engine_release(engine: &EngineRelease) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if engine.engine_kind != EngineKind::Ninfer as i32 {
        errors.push(anyhow::anyhow!(
            "ninfer engine release has a mismatched engine kind"
        ));
    }
    let valid = engine.parameter_schema.as_ref().is_some_and(|schema| {
        !schema.parameters.is_empty()
            && schema.parameters.iter().all(|definition| {
                !definition.parameter_id.is_empty()
                    && matches!(
                        definition.parameter_type.as_ref(),
                        Some(ParameterType::UnsignedInteger(range))
                            if range.maximum >= range.minimum && !range.unit.is_empty()
                    )
            })
    });
    if !valid {
        errors.push(anyhow::anyhow!(
            "ninfer engine release requires non-empty unsigned-integer parameter definitions"
        ));
    }
    let within_limits = [
        (TOTAL_CONTEXT_PARAMETER_ID, MAXIMUM_TOTAL_CONTEXT_TOKENS),
        (
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            MAXIMUM_CONTEXT_PER_REQUEST_TOKENS,
        ),
        (PARALLEL_REQUESTS_PARAMETER_ID, MAXIMUM_PARALLEL_REQUESTS),
    ]
    .into_iter()
    .all(|(parameter_id, limit)| {
        unsigned_parameter_maximum(engine, parameter_id).is_some_and(|maximum| maximum <= limit)
    });
    if !within_limits {
        errors.push(anyhow::anyhow!(
            "ninfer engine release must declare {TOTAL_CONTEXT_PARAMETER_ID} up to \
             {MAXIMUM_TOTAL_CONTEXT_TOKENS} tokens, {CONTEXT_PER_REQUEST_PARAMETER_ID} up to \
             {MAXIMUM_CONTEXT_PER_REQUEST_TOKENS} tokens and {PARALLEL_REQUESTS_PARAMETER_ID} up \
             to {MAXIMUM_PARALLEL_REQUESTS}"
        ));
    }
    let linux_amd64 = engine.artifacts.iter().all(|artifact| {
        !artifact.selectors.is_empty()
            && artifact.selectors.iter().all(|selector| {
                selector.platform.as_ref().is_some_and(|platform| {
                    platform.operating_system == "linux" && platform.architecture == "amd64"
                })
            })
    });
    if !linux_amd64 {
        errors.push(anyhow::anyhow!(
            "ninfer engine release supports only linux/amd64 artifacts"
        ));
    }
    errors
}

/// Accepts supported hardware classes that select only NVIDIA accelerators. Selectors match
/// single-device targets only, so ninfer runs on exactly one NVIDIA accelerator.
pub(crate) fn validate_hardware_support(
    configuration: &ModelEngineConfiguration,
    hardware_classes: &[&HardwareClass],
) -> Vec<anyhow::Error> {
    classes_outside_vendors(hardware_classes, &[Accelerator::Nvidia])
        .into_iter()
        .map(|hardware_class_id| {
            anyhow::anyhow!(
                "configuration {:?} hardware class {hardware_class_id:?} must select only NVIDIA accelerators for ninfer",
                configuration.configuration_id
            )
        })
        .collect()
}

pub(crate) fn validate_model_configuration(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if engine.engine_kind != EngineKind::Ninfer as i32
        || configuration
            .engine_configuration
            .as_ref()
            .is_none_or(|wrapper| wrapper.engine_kind != EngineKind::Ninfer as i32)
    {
        errors.push(anyhow::anyhow!(
            "ninfer configuration has a mismatched engine wrapper or release kind"
        ));
    }
    let Some(payload) = configuration.engine_configuration.as_ref() else {
        return errors;
    };
    let Some(Configuration::Ninfer(payload)) = payload.configuration.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a payload that does not match ninfer",
            configuration.configuration_id
        ));
        return errors;
    };

    validate_payload(model, configuration, engine, payload, &mut errors);
    errors
}

fn validate_payload(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
    payload: &NinferModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(fixed) = payload.fixed_settings.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires fixed ninfer settings",
            configuration.configuration_id
        ));
        return;
    };
    if !matches!(
        NinferKvCacheDataType::try_from(fixed.kv_cache_data_type),
        Ok(NinferKvCacheDataType::Fp8)
    ) || fixed.pending_timeout_milliseconds == 0
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid fixed ninfer settings",
            configuration.configuration_id
        ));
    }

    validate_parameters(configuration, engine, fixed, errors);
    validate_artifacts(model, configuration, errors);
    validate_execution_shape(configuration, errors);
    validate_realizations(model, configuration, payload, errors);
}

fn validate_parameters(
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
    fixed: &NinferFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let declared = engine
        .parameter_schema
        .as_ref()
        .map(|schema| {
            schema
                .parameters
                .iter()
                .map(|definition| definition.parameter_id.as_str())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let referenced = [
        fixed.device_state_slots_parameter_id.as_str(),
        fixed.host_context_mib_parameter_id.as_str(),
    ];
    let valid = referenced[0] != referenced[1]
        && referenced
            .iter()
            .all(|id| declared.contains(id) && !is_shared_parameter(id));
    if !valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid ninfer engine parameter references",
            configuration.configuration_id
        ));
    }

    // Every release parameter must reach the launch plan, so none can be set without effect.
    let launched = referenced
        .into_iter()
        .chain([
            TOTAL_CONTEXT_PARAMETER_ID,
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            PARALLEL_REQUESTS_PARAMETER_ID,
        ])
        .collect::<BTreeSet<_>>();
    if declared != launched {
        errors.push(anyhow::anyhow!(
            "configuration {:?} must reference every ninfer engine release parameter",
            configuration.configuration_id
        ));
    }
}

fn validate_artifacts(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let valid = !configuration.artifact_bindings.is_empty()
        && configuration.artifact_bindings.iter().all(|binding| {
            binding.auxiliary_artifact_ids.is_empty()
                && model
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.artifact_id == binding.model_artifact_id)
                    .and_then(|artifact| artifact.source.as_ref())
                    .and_then(|source| source.content.as_ref())
                    .is_some_and(|content| {
                        matches!(
                            content,
                            HuggingFaceContent::File(file)
                                if file.path.ends_with(MODEL_FILE_EXTENSION)
                        )
                    })
        });
    if !valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires single-file .ninfer model artifacts without auxiliary artifacts",
            configuration.configuration_id
        ));
    }
}

fn validate_execution_shape(
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let one_target = configuration
        .execution_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities.supported_parallel_shapes == [ModelParallelShape::One as i32]
        });
    if !one_target {
        errors.push(anyhow::anyhow!(
            "configuration {:?} supports only one-target ninfer execution",
            configuration.configuration_id
        ));
    }
}

fn validate_realizations(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    payload: &NinferModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    for option_id in kv_cache_precision_option_ids(model, configuration) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} option {option_id:?} is a KV cache precision option, which ninfer \
             does not support",
            configuration.configuration_id
        ));
    }

    let mut option_ids = BTreeSet::new();
    let mut vision_count = 0;
    let mut mtp_count = 0;
    for realization in &payload.option_realizations {
        let option = configuration
            .options
            .iter()
            .find(|option| option.option_id == realization.option_id);
        let model_kind = model
            .options
            .iter()
            .find(|option| option.option_id == realization.option_id)
            .and_then(|option| option.kind.as_ref());
        let valid = option_ids.insert(realization.option_id.clone())
            && option.is_some_and(|option| {
                valid_realization(
                    realization.realization.as_ref(),
                    option,
                    model_kind,
                    &mut vision_count,
                    &mut mtp_count,
                )
            });
        if !valid {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has invalid ninfer realization for option {:?}",
                configuration.configuration_id,
                realization.option_id
            ));
        } else if let (Some(Realization::Mtp(_)), Some(maximum)) = (
            realization.realization.as_ref(),
            option.and_then(declared_token_count_maximum),
        ) && maximum > MAXIMUM_MTP_TOKEN_COUNT
        {
            errors.push(anyhow::anyhow!(
                "configuration {:?} option {:?} declares up to {maximum} speculative tokens, \
                 above the ninfer MTP maximum of {MAXIMUM_MTP_TOKEN_COUNT}",
                configuration.configuration_id,
                realization.option_id
            ));
        }
    }
    if option_ids.len() != configuration.options.len()
        || configuration
            .options
            .iter()
            .any(|option| !option_ids.contains(&option.option_id))
        || vision_count > 1
        || mtp_count > 1
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires one unambiguous ninfer realization per model option",
            configuration.configuration_id
        ));
    }

    let image_capable = configuration
        .input_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities
                .supported_modalities
                .contains(&(ModelInputModality::Image as i32))
                && capabilities.image_input_capacity.is_some()
        });
    if (vision_count == 1) != image_capable {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a ninfer vision capability mismatch",
            configuration.configuration_id
        ));
    }
}

fn valid_realization(
    realization: Option<&Realization>,
    option: &ModelConfigurationOption,
    model_kind: Option<&ModelOptionKind>,
    vision_count: &mut usize,
    mtp_count: &mut usize,
) -> bool {
    match (model_kind, realization) {
        (Some(ModelOptionKind::Vision(_)), Some(Realization::Vision(vision))) => {
            *vision_count += 1;
            parameter_has_type(option, &vision.enabled_parameter_id, |kind| {
                matches!(kind, ModelOptionParameterType::Boolean(_))
            })
        }
        (Some(ModelOptionKind::SpeculativeDecoding(kind)), Some(Realization::Mtp(_))) => {
            *mtp_count += 1;
            kind.method == SpeculativeDecodingMethod::Mtp as i32
                && parameter_has_type(option, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID, |kind| {
                    matches!(kind, ModelOptionParameterType::UnsignedInteger(_))
                })
        }
        _ => false,
    }
}

/// Reads the declared maximum of an option's speculative-token count.
fn declared_token_count_maximum(option: &ModelConfigurationOption) -> Option<u64> {
    option
        .parameters
        .iter()
        .find(|parameter| parameter.parameter_id == SPECULATIVE_TOKEN_COUNT_PARAMETER_ID)
        .and_then(|parameter| match parameter.parameter_type.as_ref() {
            Some(ModelOptionParameterType::UnsignedInteger(range)) => Some(range.maximum),
            _ => None,
        })
}

fn parameter_has_type(
    option: &ModelConfigurationOption,
    parameter_id: &str,
    predicate: impl FnOnce(&ModelOptionParameterType) -> bool,
) -> bool {
    option
        .parameters
        .iter()
        .find(|parameter| parameter.parameter_id == parameter_id)
        .and_then(|parameter| parameter.parameter_type.as_ref())
        .is_some_and(predicate)
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::{CatalogModelId, test_support::vendor_hardware_class};
    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::{
        catalog::v1::{
            BooleanEngineParameterSchema, EngineParameterDefinition, HuggingFaceArtifactSource,
            HuggingFaceFileArtifact, HuggingFaceSnapshotArtifact, SpeculativeDecodingModelOption,
            UnsignedIntegerEngineParameterSchema,
        },
        model::v1::{
            LocalModelDeploymentConfiguration, ResolvedLocalModelDeploymentSnapshot,
            ResolvedSizing, portable_model_launch_argument_token::Value as ArgumentValue,
        },
    };

    use super::*;
    use crate::{
        model::resolution::{
            ModelLaunchResolutionError, apply_profile_defaults, resolve_local_launch,
        },
        test_support::{
            ninfer::{configuration, model, release},
            unsigned_parameter_values,
        },
    };

    #[test]
    fn accepts_the_synthetic_release_and_configuration() {
        assert!(validate_engine_release(&release()).is_empty());
        assert!(
            validate_model_configuration(&model(), &configuration(), &release()).is_empty(),
            "{:?}",
            validate_model_configuration(&model(), &configuration(), &release())
        );
    }

    #[test]
    fn rejects_release_with_wrong_kind_or_unsupported_parameter_schema() {
        let mut wrong_kind = release();
        wrong_kind.engine_kind = EngineKind::Vllm as i32;
        assert_eq!(
            messages(validate_engine_release(&wrong_kind)),
            ["ninfer engine release has a mismatched engine kind"]
        );

        let mut boolean = release();
        boolean
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters[3]
            .parameter_type = Some(ParameterType::Boolean(BooleanEngineParameterSchema {}));
        assert_eq!(
            messages(validate_engine_release(&boolean)),
            ["ninfer engine release requires non-empty unsigned-integer parameter definitions"]
        );
    }

    #[test]
    fn rejects_shared_parameters_missing_or_above_ninfer_limits() {
        let expected = [
            "ninfer engine release must declare total-context up to 2097152 tokens, context-per-request up to 262144 tokens and parallel-requests up to 8",
        ];
        for (parameter_id, maximum) in [
            (TOTAL_CONTEXT_PARAMETER_ID, Some(2_097_153)),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, Some(262_145)),
            (PARALLEL_REQUESTS_PARAMETER_ID, Some(9)),
            (PARALLEL_REQUESTS_PARAMETER_ID, None),
        ] {
            let mut release = release();
            let parameters = &mut release
                .parameter_schema
                .as_mut()
                .expect("schema")
                .parameters;
            match maximum {
                Some(maximum) => {
                    let definition = parameters
                        .iter_mut()
                        .find(|definition| definition.parameter_id == parameter_id)
                        .expect("parameter");
                    definition.parameter_type = Some(unsigned(1, maximum, "units"));
                }
                None => parameters.retain(|definition| definition.parameter_id != parameter_id),
            }

            assert_eq!(
                messages(validate_engine_release(&release)),
                expected,
                "{parameter_id} {maximum:?}"
            );
        }
    }

    #[test]
    fn rejects_extra_parameter_references_that_are_missing_or_shared() {
        for reference in ["missing", TOTAL_CONTEXT_PARAMETER_ID] {
            let mut configuration = configuration();
            fixed_settings(&mut configuration).host_context_mib_parameter_id = reference.to_owned();

            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration,
                    &release()
                )),
                [
                    "configuration \"ninfer-nvidia\" has invalid ninfer engine parameter references",
                    "configuration \"ninfer-nvidia\" must reference every ninfer engine release parameter",
                ],
                "{reference}"
            );
        }
    }

    #[test]
    fn rejects_release_parameters_the_configuration_does_not_reference() {
        let mut release = release();
        release
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .push(EngineParameterDefinition {
                parameter_id: "prefill-chunk".to_owned(),
                parameter_type: Some(unsigned(128, 8192, "tokens")),
                ..Default::default()
            });

        assert!(validate_engine_release(&release).is_empty());
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &configuration(),
                &release
            )),
            [
                "configuration \"ninfer-nvidia\" must reference every ninfer engine release parameter"
            ]
        );
    }

    #[test]
    fn rejects_release_artifacts_other_than_linux_amd64() {
        for (operating_system, architecture) in [("linux", "arm64"), ("darwin", "amd64")] {
            let mut release = release();
            release
                .artifacts
                .push(dotcore_domain::test_support::oci::engine_artifact(
                    "other",
                    Some("registry.dotcore.test"),
                    "ninfer",
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                    operating_system,
                    architecture,
                    "nvidia",
                ));

            assert_eq!(
                messages(validate_engine_release(&release)),
                ["ninfer engine release supports only linux/amd64 artifacts"],
                "{operating_system}/{architecture}"
            );
        }
    }

    #[test]
    fn rejects_invalid_fixed_settings() {
        let mut configuration = configuration();
        let fixed = fixed_settings(&mut configuration);
        fixed.kv_cache_data_type = NinferKvCacheDataType::Unspecified as i32;
        fixed.pending_timeout_milliseconds = 0;

        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &configuration,
                &release()
            )),
            ["configuration \"ninfer-nvidia\" has invalid fixed ninfer settings"]
        );
    }

    #[test]
    fn rejects_wrong_artifact_extension_snapshot_artifact_and_auxiliary_artifact() {
        let expected = [
            "configuration \"ninfer-nvidia\" requires single-file .ninfer model artifacts without auxiliary artifacts",
        ];

        let mut gguf = model();
        gguf.artifacts[0].source = Some(file_source("weights.gguf"));
        assert_eq!(
            messages(validate_model_configuration(
                &gguf,
                &configuration(),
                &release()
            )),
            expected
        );

        let mut snapshot = model();
        snapshot.artifacts[0].source = Some(HuggingFaceArtifactSource {
            repository: "example/weights".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            content: Some(HuggingFaceContent::Snapshot(
                HuggingFaceSnapshotArtifact::default(),
            )),
        });
        assert_eq!(
            messages(validate_model_configuration(
                &snapshot,
                &configuration(),
                &release()
            )),
            expected
        );

        let mut auxiliary = configuration();
        auxiliary.artifact_bindings[0]
            .auxiliary_artifact_ids
            .push("template".to_owned());
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &auxiliary,
                &release()
            )),
            expected
        );
    }

    #[test]
    fn rejects_other_than_one_target() {
        let mut two_targets = configuration();
        two_targets
            .execution_capabilities
            .as_mut()
            .expect("execution capabilities")
            .supported_parallel_shapes = Vec::new();

        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &two_targets,
                &release()
            )),
            ["configuration \"ninfer-nvidia\" supports only one-target ninfer execution"]
        );
    }

    #[test]
    fn hardware_classes_must_select_only_nvidia() {
        let nvidia = vendor_hardware_class("nvidia");
        let apple = vendor_hardware_class("apple");

        assert!(validate_hardware_support(&configuration(), &[&nvidia]).is_empty());
        assert_eq!(
            messages(validate_hardware_support(
                &configuration(),
                &[&nvidia, &apple]
            )),
            [
                "configuration \"ninfer-nvidia\" hardware class \"apple\" must select only NVIDIA accelerators for ninfer"
            ]
        );
    }

    #[test]
    fn rejects_unknown_option_and_token_counts_above_the_ninfer_limit() {
        let mut unknown = configuration();
        payload(&mut unknown).option_realizations[1].option_id = "unknown".to_owned();
        assert_eq!(
            messages(validate_model_configuration(&model(), &unknown, &release())),
            [
                "configuration \"ninfer-nvidia\" has invalid ninfer realization for option \"unknown\"",
                "configuration \"ninfer-nvidia\" requires one unambiguous ninfer realization per model option",
            ]
        );

        let mut draft = configuration();
        draft.options[1].parameters[0].parameter_type =
            Some(ModelOptionParameterType::UnsignedInteger(
                dotcore_proto::dotcore::catalog::v1::UnsignedIntegerModelOptionParameterSchema {
                    minimum: 0,
                    maximum: 6,
                    unit: "tokens".to_owned(),
                },
            ));
        assert_eq!(
            messages(validate_model_configuration(&model(), &draft, &release())),
            [
                "configuration \"ninfer-nvidia\" option \"mtp\" declares up to 6 speculative tokens, above the ninfer MTP maximum of 5"
            ]
        );
    }

    #[test]
    fn rejects_a_kv_cache_precision_option() {
        let mut model = model();
        model
            .options
            .push(dotcore_proto::dotcore::catalog::v1::ModelOption {
                option_id: "kv-cache".to_owned(),
                display_name: "KV cache precision".to_owned(),
                kind: Some(ModelOptionKind::KvCachePrecision(
                    dotcore_proto::dotcore::catalog::v1::KvCachePrecisionModelOption {},
                )),
            });
        let mut configuration = configuration();
        configuration.options.push(ModelConfigurationOption {
            option_id: "kv-cache".to_owned(),
            ..Default::default()
        });

        assert_eq!(
            messages(validate_model_configuration(
                &model,
                &configuration,
                &release()
            )),
            [
                "configuration \"ninfer-nvidia\" option \"kv-cache\" is a KV cache precision option, which ninfer does not support",
                "configuration \"ninfer-nvidia\" requires one unambiguous ninfer realization per model option",
            ]
        );
    }

    #[test]
    fn mtp_realization_requires_an_mtp_speculative_option() {
        assert!(
            messages(validate_model_configuration(
                &model(),
                &configuration(),
                &release()
            ))
            .is_empty()
        );

        let mut model = model();
        model.options[1].kind = Some(ModelOptionKind::SpeculativeDecoding(
            SpeculativeDecodingModelOption {
                method: SpeculativeDecodingMethod::Unspecified as i32,
            },
        ));
        assert_eq!(
            messages(validate_model_configuration(
                &model,
                &configuration(),
                &release()
            )),
            ["configuration \"ninfer-nvidia\" has invalid ninfer realization for option \"mtp\""]
        );
    }

    #[test]
    fn rejects_mismatched_payload() {
        let mut configuration = configuration();
        configuration
            .engine_configuration
            .as_mut()
            .expect("wrapper")
            .configuration = Some(Configuration::Vllm(Default::default()));

        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &configuration,
                &release()
            )),
            ["configuration \"ninfer-nvidia\" has a payload that does not match ninfer"]
        );
    }

    #[test]
    fn catalog_loads_a_profile_shaped_ninfer_declaration() {
        let catalog = profile_shaped_catalog();

        let model = catalog
            .list_model_releases()
            .into_iter()
            .find(|model| model.model_id == "fixture-ninfer-model")
            .expect("ninfer model");
        assert_eq!(
            model.configurations[0].engine_configuration,
            configuration().engine_configuration
        );
        assert_eq!(
            catalog.list_engine_releases()[0].parameter_schema,
            release().parameter_schema
        );
    }

    #[test]
    fn engine_defaults_launch_one_total_context_shared_by_four_requests() {
        let catalog = profile_shaped_catalog();

        let launch = launch(&catalog, &[]).expect("ninfer launch at engine defaults");

        assert_eq!(
            launch_argv(&launch),
            [
                "/usr/local/bin/ninfer-serve",
                "--model-id",
                "fixture-ninfer-model",
                "--host",
                "0.0.0.0",
                "--port",
                "8080",
                "--device",
                "0",
                "--vision",
                "--max-context",
                "32768",
                "--kv-capacity",
                "32768",
                "--spec",
                "mtp",
                "--draft-tokens",
                "3",
                "--max-concurrency",
                "4",
                "--pending-timeout-ms",
                "120000",
                "--kv-dtype",
                "fp8",
                "--device-state-slots",
                "4",
                "--host-context-mib",
                "20480",
                "--lm-head-draft",
                "--preserve-thinking",
            ]
        );
        // 1000 + 10 × 32768 + 100 × 4 + (500 + 2 × 32768) for the MTP draft cache.
        assert_eq!(
            launch.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(395_116),
                derived_engine_parameter_ids: vec![CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned()],
                host_memory_bytes: Some(20_480 * 1_048_576),
            })
        );
    }

    #[test]
    fn a_total_context_within_parallel_requests_at_the_context_per_request_launches() {
        let catalog = profile_shaped_catalog();

        let shared = launch(
            &catalog,
            &[
                (TOTAL_CONTEXT_PARAMETER_ID, 524_288),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 131_072),
            ],
        )
        .expect("total context at four full requests");
        assert_eq!(
            launch_flags(&shared),
            [
                ("--max-context", "131072".to_owned()),
                ("--kv-capacity", "524288".to_owned()),
                ("--max-concurrency", "4".to_owned()),
            ]
        );
        assert_eq!(
            shared.sizing.as_ref().map(|sizing| (
                sizing.accelerator_memory_bytes,
                sizing.derived_engine_parameter_ids.clone()
            )),
            Some((Some(6_293_356), Vec::new()))
        );

        let filled = launch(&catalog, &[(TOTAL_CONTEXT_PARAMETER_ID, 1_048_576)])
            .expect("context per request filled at the model maximum");
        assert_eq!(
            launch_flags(&filled),
            [
                ("--max-context", "262144".to_owned()),
                ("--kv-capacity", "1048576".to_owned()),
                ("--max-concurrency", "4".to_owned()),
            ]
        );
    }

    #[test]
    fn rejects_a_total_context_above_parallel_requests_at_the_context_per_request() {
        let catalog = profile_shaped_catalog();

        assert_eq!(
            launch(
                &catalog,
                &[
                    (TOTAL_CONTEXT_PARAMETER_ID, 524_289),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 131_072),
                ],
            )
            .expect_err("total context above every request at full context")
            .to_string(),
            "invalid engine parameter values: total context of 524289 tokens is above the 524288 tokens that 4 parallel requests can use at this context per request; lower the total context, or raise the parallel requests or the context per request"
        );
        assert_eq!(
            launch(
                &catalog,
                &[
                    (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 1),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
                ],
            )
            .expect_err("total context above one request")
            .to_string(),
            "invalid engine parameter values: total context of 65536 tokens is above the 32768 tokens that 1 parallel requests can use at this context per request; lower the total context, or raise the parallel requests or the context per request"
        );
    }

    #[test]
    fn rejects_a_context_per_request_above_the_total_context() {
        let catalog = profile_shaped_catalog();

        assert_eq!(
            launch(
                &catalog,
                &[
                    (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_769),
                ],
            )
            .expect_err("context per request above the total context")
            .to_string(),
            "invalid engine parameter values: context per request of 32769 tokens is above the total context of 32768 tokens; lower the context per request or raise the total context"
        );
    }

    const CATALOG_ENGINE: &str = r#"
engineReleaseId: "fixture-ninfer"
engineKind: "ENGINE_KIND_NINFER"
version: "1"
parameterSchema:
  schemaId: "fixture-ninfer-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, description: "total-context", unsignedInteger: { minimum: "1024", maximum: "2097152", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, description: "context-per-request", unsignedInteger: { minimum: "1024", maximum: "262144", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, description: "parallel-requests", unsignedInteger: { minimum: "1", maximum: "8", unit: "requests", defaultValue: "4" } }
    - { parameterId: "host-context-mib", displayName: "Host context memory", exposed: true, description: "host-context-mib", unsignedInteger: { minimum: "0", maximum: "1048576", unit: "MiB" } }
    - { parameterId: "device-state-slots", displayName: "Device state slots", exposed: true, description: "device-state-slots", unsignedInteger: { minimum: "0", maximum: "64", unit: "slots" } }
artifacts:
  - artifactId: "linux-amd64-cuda"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "ninfer" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

    const CATALOG_MODEL: &str = r#"
modelId: "fixture-ninfer-model"
displayName: "Fixture ninfer model"
version: "1"
maximumContextTokens: 262144
options:
  - { optionId: "vision", displayName: "Vision", vision: {} }
  - { optionId: "mtp", displayName: "MTP", speculativeDecoding: { method: "SPECULATIVE_DECODING_METHOD_MTP" } }
artifacts:
  - artifactId: "fixture-ninfer-artifact"
    weightQuantization: "NVFP4"
    source:
      repository: "dotcore/fixture-ninfer"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture_nvfp4.ninfer"
        provenanceDigest: "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
configurations:
  - configurationId: "ninfer-nvidia"
    engineArtifacts: [{ engineReleaseId: "fixture-ninfer", engineArtifactId: "linux-amd64-cuda" }]
    artifactBindings: [{ bindingId: "fixture-ninfer-artifact", modelArtifactId: "fixture-ninfer-artifact" }]
    sizing:
      parameters: { baseBytes: 1000, bytesPerKvToken: 10, bytesPerConcurrency: 100, mtpBaseBytes: 500, mtpBytesPerKvToken: 2 }
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: >-
            ${baseBytes} + ${bytesPerKvToken} * ${total-context}
            + ${bytesPerConcurrency} * ${parallel-requests}
            + if(${options.mtp.token-count} > 0, 1, 0) * (${mtpBaseBytes} + ${mtpBytesPerKvToken} * ${total-context})
        hostMemory:
          unit: bytes
          equation: "${host-context-mib} * MiB"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        hardwareRequirements:
          minimumMemoryBytesPerDevice: "32000000000"
          minimumSystemMemoryBytes: "64000000000"
        bindingSizing:
          - bindingId: "fixture-ninfer-artifact"
    engineConfiguration:
      engineKind: "ENGINE_KIND_NINFER"
      ninfer:
        fixedSettings:
          kvCacheDataType: "NINFER_KV_CACHE_DATA_TYPE_FP8"
          pendingTimeoutMilliseconds: 120000
          preserveThinking: true
          deviceStateSlotsParameterId: "device-state-slots"
          hostContextMibParameterId: "host-context-mib"
        optionRealizations:
          - optionId: "vision"
            vision: { enabledParameterId: "enabled" }
          - optionId: "mtp"
            mtp: { lmHeadDraft: true }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT", "MODEL_INPUT_MODALITY_IMAGE"]
      imageInputCapacity:
        maximumImageCount: 4
        supportedMediaTypes: ["MODEL_IMAGE_MEDIA_TYPE_JPEG", "MODEL_IMAGE_MEDIA_TYPE_PNG"]
        maximumEncodedBytes: "20971520"
        maximumDecodedBytes: "67108864"
        maximumWidth: 8192
        maximumHeight: 8192
        maximumPixels: "16000000"
        fixedVisualTokens: 256
        fixedDetail: "MODEL_IMAGE_DETAIL_AUTO"
    options:
      - optionId: "vision"
        parameters: [{ parameterId: "enabled", displayName: "enabled", boolean: {} }]
        defaultValues: { optionId: "vision", values: [{ parameterId: "enabled", boolean: true }] }
      - optionId: "mtp"
        parameters:
          - { parameterId: "token-count", displayName: "Speculative tokens", unsignedInteger: { minimum: "0", maximum: "5", unit: "tokens" } }
        defaultValues:
          optionId: "mtp"
          values:
            - { parameterId: "token-count", unsignedInteger: "3" }
profiles:
  - profileId: "ninfer-nvfp4"
    displayName: "NVFP4 ninfer"
    description: "Runs the fixture through ninfer."
    configurationId: "ninfer-nvidia"
    defaultModelArtifactBindingId: "fixture-ninfer-artifact"
    engineParameterDefaults:
      values:
        - { parameterId: "parallel-requests", unsignedInteger: "4" }
        - { parameterId: "host-context-mib", unsignedInteger: "20480" }
        - { parameterId: "device-state-slots", unsignedInteger: "4" }
    "#;

    /// Loads the profile-shaped engine and model with the shared hardware classes.
    fn profile_shaped_catalog() -> dotcore_catalog::Catalog {
        let root = tempfile::tempdir().expect("Catalog root");
        for (path, content) in [
            ("engines/ninfer.yaml", CATALOG_ENGINE),
            ("models/ninfer-model.yaml", CATALOG_MODEL),
        ] {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().expect("directory")).expect("directory");
            std::fs::write(path, content).expect("declaration");
        }
        dotcore_catalog::test_support::write_hardware_classes(root.path());

        dotcore_catalog::Catalog::load_with_policy(
            root.path(),
            dotcore_catalog::OciArtifactPolicy::Development,
            crate::model::engine::declaration_validator(),
        )
        .expect("ninfer catalog")
    }

    /// Resolves the profile on an NVIDIA target with the supplied engine parameter values after
    /// the profile and engine defaults fill the rest, as the controller does.
    fn launch(
        catalog: &dotcore_catalog::Catalog,
        values: &[(&str, u64)],
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
        let model_id = CatalogModelId("fixture-ninfer-model".to_owned());
        let configuration = apply_profile_defaults(
            catalog,
            &model_id,
            "ninfer-nvfp4",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "fixture-ninfer-artifact".to_owned(),
                engine_parameter_values: Some(unsigned_parameter_values(values.iter().copied())),
                ..Default::default()
            },
        )?;
        let (target, inventory) =
            crate::test_support::execution_target_capabilities(ExecutionPlatform::linux_amd64());

        resolve_local_launch(
            catalog,
            &model_id,
            "ninfer-nvfp4",
            &configuration,
            &target,
            &inventory,
            None,
        )
    }

    /// Literal arguments of the launch plan, without the model binding.
    fn launch_argv(launch: &ResolvedLocalModelDeploymentSnapshot) -> Vec<&str> {
        launch
            .desired_launch_plan
            .as_ref()
            .expect("launch plan")
            .arguments
            .iter()
            .filter_map(|argument| match argument.value.as_ref() {
                Some(ArgumentValue::Literal(value)) => Some(value.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Context per request, total context and parallel request flags of the launch plan, each
    /// with its value.
    fn launch_flags(launch: &ResolvedLocalModelDeploymentSnapshot) -> Vec<(&'static str, String)> {
        let argv = launch_argv(launch);
        ["--max-context", "--kv-capacity", "--max-concurrency"]
            .into_iter()
            .map(|flag| {
                let position = argv
                    .iter()
                    .position(|argument| *argument == flag)
                    .unwrap_or_else(|| panic!("{flag} in the launch plan"));
                (flag, argv[position + 1].to_owned())
            })
            .collect()
    }

    fn messages(errors: Vec<anyhow::Error>) -> Vec<String> {
        errors.into_iter().map(|error| error.to_string()).collect()
    }

    fn unsigned(minimum: u64, maximum: u64, unit: &str) -> ParameterType {
        ParameterType::UnsignedInteger(UnsignedIntegerEngineParameterSchema {
            minimum,
            maximum,
            unit: unit.to_owned(),
            default_value: None,
        })
    }

    fn file_source(path: &str) -> HuggingFaceArtifactSource {
        HuggingFaceArtifactSource {
            repository: "example/weights".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            content: Some(HuggingFaceContent::File(HuggingFaceFileArtifact {
                path: path.to_owned(),
                provenance_digest: "0".repeat(64),
            })),
        }
    }

    fn payload(configuration: &mut ModelEngineConfiguration) -> &mut NinferModelConfiguration {
        match configuration
            .engine_configuration
            .as_mut()
            .and_then(|wrapper| wrapper.configuration.as_mut())
        {
            Some(Configuration::Ninfer(payload)) => payload,
            _ => panic!("ninfer payload"),
        }
    }

    fn fixed_settings(
        configuration: &mut ModelEngineConfiguration,
    ) -> &mut NinferFixedModelSettings {
        payload(configuration)
            .fixed_settings
            .as_mut()
            .expect("fixed settings")
    }
}
