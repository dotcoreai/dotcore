//! vLLM-specific validation of catalog engine releases and model engine configurations.
//!
//! Each check returns the declarations that fail, covering the release engine kind and its parameter
//! schema, and for configurations the wrapper and release kinds, the fixed settings, the engine parameter
//! references, the snapshot artifact bindings, the one-target execution shape, and one unambiguous
//! realization per model option, including where an MTP head comes from. The shared context
//! parameters need no reference: the plan reads them directly, and the catalog checks their shape.

use std::collections::BTreeSet;

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID, TOTAL_CONTEXT_PARAMETER_ID,
    is_shared_parameter,
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, HardwareClass, Model, ModelConfigurationOption, ModelEngineConfiguration,
        ModelInputModality, ModelParallelShape, SpeculativeDecodingMethod,
        engine_parameter_definition::ParameterType,
        hugging_face_artifact_source::Content as HuggingFaceContent,
        model_option::Kind as ModelOptionKind,
        model_option_parameter_definition::ParameterType as ModelOptionParameterType,
    },
    engine::{
        v1::engine_model_configuration::Configuration,
        vllm::v1::{
            VllmKvCacheDataType, VllmKvOffloadingBackend, VllmModelConfiguration,
            VllmReadinessMode, vllm_option_realization::Realization,
        },
    },
};

use crate::model::engine::{
    declaration::{classes_outside_vendors, kv_cache_precision_option_ids},
    definition::unsigned_parameter_maximum,
    speculative_draft::{self, SpeculativeDraft},
};

/// Largest total context a vLLM release may declare, in tokens.
const MAXIMUM_TOTAL_CONTEXT_TOKENS: u64 = 16_777_216;
/// Largest context per request a vLLM release may declare, in tokens.
const MAXIMUM_CONTEXT_PER_REQUEST_TOKENS: u64 = 1_048_576;
/// Largest parallel request count a vLLM release may declare.
const MAXIMUM_PARALLEL_REQUESTS: u64 = 1024;

/// Accepts a release of non-empty unsigned-integer or boolean parameters that declares the shared
/// context parameters within vLLM's limits. The catalog checks the shared parameters' fixed shape.
pub(crate) fn validate_engine_release(engine: &EngineRelease) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if engine.engine_kind != dotcore_proto::dotcore::engine::v1::EngineKind::Vllm as i32 {
        errors.push(anyhow::anyhow!(
            "vLLM engine release has a mismatched engine kind"
        ));
    }
    let valid = engine.parameter_schema.as_ref().is_some_and(|schema| {
        !schema.parameters.is_empty()
            && schema.parameters.iter().all(|definition| {
                !definition.parameter_id.is_empty()
                    && match definition.parameter_type.as_ref() {
                        Some(ParameterType::UnsignedInteger(range)) => {
                            range.maximum >= range.minimum && !range.unit.is_empty()
                        }
                        Some(ParameterType::Boolean(_)) => true,
                        None => false,
                    }
            })
    });
    if !valid {
        errors.push(anyhow::anyhow!(
            "vLLM engine release requires non-empty supported parameter definitions"
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
            "vLLM engine release must declare {TOTAL_CONTEXT_PARAMETER_ID} up to \
             {MAXIMUM_TOTAL_CONTEXT_TOKENS} tokens, {CONTEXT_PER_REQUEST_PARAMETER_ID} up to \
             {MAXIMUM_CONTEXT_PER_REQUEST_TOKENS} tokens and {PARALLEL_REQUESTS_PARAMETER_ID} up \
             to {MAXIMUM_PARALLEL_REQUESTS}"
        ));
    }
    errors
}

/// Accepts supported hardware classes that select only NVIDIA accelerators. Selectors match
/// single-device targets only, so vLLM runs on exactly one NVIDIA accelerator.
pub(crate) fn validate_hardware_support(
    configuration: &ModelEngineConfiguration,
    hardware_classes: &[&HardwareClass],
) -> Vec<anyhow::Error> {
    classes_outside_vendors(hardware_classes, &[Accelerator::Nvidia])
        .into_iter()
        .map(|hardware_class_id| {
            anyhow::anyhow!(
                "configuration {:?} hardware class {hardware_class_id:?} must select only NVIDIA accelerators for vLLM",
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
    if engine.engine_kind != dotcore_proto::dotcore::engine::v1::EngineKind::Vllm as i32
        || configuration
            .engine_configuration
            .as_ref()
            .is_none_or(|wrapper| {
                wrapper.engine_kind != dotcore_proto::dotcore::engine::v1::EngineKind::Vllm as i32
            })
    {
        errors.push(anyhow::anyhow!(
            "vLLM configuration has a mismatched engine wrapper or release kind"
        ));
    }
    let Some(payload) = configuration.engine_configuration.as_ref() else {
        return errors;
    };
    let Some(Configuration::Vllm(payload)) = payload.configuration.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a payload that does not match vLLM",
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
    payload: &VllmModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(fixed) = payload.fixed_settings.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires fixed vLLM settings",
            configuration.configuration_id
        ));
        return;
    };

    if fixed.container_port == 0
        || fixed.container_port > u16::MAX as u32
        || !matches!(
            VllmKvCacheDataType::try_from(fixed.kv_cache_data_type),
            Ok(VllmKvCacheDataType::Fp8)
        )
        || !matches!(
            VllmKvOffloadingBackend::try_from(fixed.kv_offloading_backend),
            Ok(VllmKvOffloadingBackend::Native)
        )
        || !matches!(
            fixed
                .readiness_policy
                .as_ref()
                .and_then(|policy| VllmReadinessMode::try_from(policy.mode).ok()),
            Some(VllmReadinessMode::Models | VllmReadinessMode::ReadyAndModels)
        )
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid fixed vLLM settings",
            configuration.configuration_id
        ));
    }

    validate_parameters(configuration, engine, fixed, errors);
    validate_artifacts(model, configuration, fixed, errors);
    validate_execution_shape(configuration, errors);
    validate_realizations(model, configuration, payload, errors);
}

fn validate_parameters(
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
    fixed: &dotcore_proto::dotcore::engine::vllm::v1::VllmFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let referenced = [
        fixed.kv_cache_memory_bytes_parameter_id.as_str(),
        fixed.maximum_batched_tokens_parameter_id.as_str(),
        fixed.maximum_cuda_graph_capture_size_parameter_id.as_str(),
        fixed.kv_offloading_size_gib_parameter_id.as_str(),
    ];
    let schema = engine.parameter_schema.as_ref();
    let valid = referenced.iter().all(|id| {
        !id.is_empty()
            && !is_shared_parameter(id)
            && schema.is_some_and(|schema| {
                schema.parameters.iter().any(|definition| {
                    definition.parameter_id == *id
                        && matches!(
                            definition.parameter_type.as_ref(),
                            Some(ParameterType::UnsignedInteger(_))
                        )
                })
            })
    });
    if !valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid vLLM engine parameter references",
            configuration.configuration_id
        ));
    }
}

fn validate_artifacts(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    fixed: &dotcore_proto::dotcore::engine::vllm::v1::VllmFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let template_exists = !fixed.chat_template_auxiliary_artifact_id.is_empty()
        && model.auxiliary_artifacts.iter().any(|artifact| {
            artifact.auxiliary_artifact_id == fixed.chat_template_auxiliary_artifact_id
        });
    let bindings_valid = !configuration.artifact_bindings.is_empty()
        && configuration.artifact_bindings.iter().all(|binding| {
            let snapshot = model
                .artifacts
                .iter()
                .find(|artifact| artifact.artifact_id == binding.model_artifact_id)
                .and_then(|artifact| artifact.source.as_ref())
                .and_then(|source| source.content.as_ref())
                .is_some_and(|content| matches!(content, HuggingFaceContent::Snapshot(_)));
            snapshot
                && binding
                    .auxiliary_artifact_ids
                    .iter()
                    .any(|id| id == &fixed.chat_template_auxiliary_artifact_id)
        });
    if !template_exists || !bindings_valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires snapshot bindings with its vLLM chat template",
            configuration.configuration_id
        ));
    }
}

fn validate_execution_shape(
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let valid = configuration
        .execution_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities.supported_parallel_shapes == [ModelParallelShape::One as i32]
        });
    if !valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} supports only one-target vLLM execution",
            configuration.configuration_id
        ));
    }
}

fn validate_realizations(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    payload: &VllmModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    for option_id in kv_cache_precision_option_ids(model, configuration) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} option {option_id:?} is a KV cache precision option, which vLLM \
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
                    model,
                    configuration,
                    realization.realization.as_ref(),
                    option,
                    model_kind,
                    &mut vision_count,
                    &mut mtp_count,
                )
            });
        if !valid {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has invalid vLLM realization for option {:?}",
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
            "configuration {:?} requires one unambiguous vLLM realization per model option",
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
            "configuration {:?} has a vLLM vision capability mismatch",
            configuration.configuration_id
        ));
    }
}

fn valid_realization(
    model: &Model,
    configuration: &ModelEngineConfiguration,
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
        (Some(ModelOptionKind::SpeculativeDecoding(kind)), Some(Realization::Mtp(mtp))) => {
            *mtp_count += 1;
            kind.method == SpeculativeDecodingMethod::Mtp as i32
                && valid_mtp_draft(
                    model,
                    configuration,
                    speculative_draft::vllm(mtp.draft.as_ref()),
                )
        }
        _ => false,
    }
}

/// Accepts an embedded head, or a draft checkpoint the model declares as a snapshot auxiliary
/// artifact that no binding lists: realization selects binding auxiliaries unconditionally, which
/// would fetch and mount the draft with speculation off.
fn valid_mtp_draft(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    draft: Option<SpeculativeDraft<'_>>,
) -> bool {
    match draft {
        Some(SpeculativeDraft::Embedded) => true,
        Some(SpeculativeDraft::Artifact(artifact_id)) => {
            let snapshot = model
                .auxiliary_artifacts
                .iter()
                .find(|artifact| artifact.auxiliary_artifact_id == artifact_id)
                .and_then(|artifact| artifact.source.as_ref())
                .and_then(|source| source.content.as_ref())
                .is_some_and(|content| matches!(content, HuggingFaceContent::Snapshot(_)));
            snapshot
                && configuration.artifact_bindings.iter().all(|binding| {
                    !binding
                        .auxiliary_artifact_ids
                        .iter()
                        .any(|id| id == artifact_id)
                })
        }
        None => false,
    }
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
            BooleanEngineParameterSchema, EngineParameterDefinition, EngineParameterSchema,
            EngineRelease, HuggingFaceArtifactSource, Model, ModelArtifactBinding,
            ModelAuxiliaryArtifact, ModelExecutionCapabilities, SpeculativeDecodingModelOption,
            UnsignedIntegerEngineParameterSchema,
        },
        engine::v1::{
            EngineKind, EngineModelConfiguration, engine_model_configuration::Configuration,
        },
        engine::vllm::v1::{
            VllmEmbeddedMtpHead, VllmFixedModelSettings, VllmModelConfiguration,
            VllmMtpDraftSource, VllmMtpOptionRealization, vllm_mtp_draft_source::Source,
        },
        model::v1::{
            LocalModelDeploymentConfiguration, ResolvedLocalModelDeploymentSnapshot,
            ResolvedSizing, portable_model_launch_argument_token::Value as ArgumentValue,
        },
    };
    use dotcore_site::node::target_inventory::{
        NodeAcceleratorObservation, RuntimePlatformObservation, TargetCapabilityReconciler,
    };

    use super::*;
    use crate::{
        model::resolution::{
            ModelLaunchResolutionError, apply_profile_defaults, resolve_local_launch,
        },
        test_support::unsigned_parameter_values,
    };

    fn release(kind: EngineKind) -> EngineRelease {
        let unsigned = |parameter_id: &str, maximum: u64, unit: &str| EngineParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            parameter_type: Some(ParameterType::UnsignedInteger(
                UnsignedIntegerEngineParameterSchema {
                    minimum: 1,
                    maximum,
                    unit: unit.to_owned(),
                    default_value: None,
                },
            )),
            ..Default::default()
        };
        EngineRelease {
            engine_kind: kind as i32,
            parameter_schema: Some(EngineParameterSchema {
                schema_id: "synthetic-schema".to_owned(),
                parameters: vec![
                    unsigned("memory", 10, "bytes"),
                    unsigned(TOTAL_CONTEXT_PARAMETER_ID, 16_777_216, "tokens"),
                    unsigned(CONTEXT_PER_REQUEST_PARAMETER_ID, 1_048_576, "tokens"),
                    unsigned(PARALLEL_REQUESTS_PARAMETER_ID, 1024, "requests"),
                ],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn accepts_synthetic_vllm_release_and_rejects_invalid_kind_or_schema() {
        assert!(validate_engine_release(&release(EngineKind::Vllm)).is_empty());
        let mut with_boolean = release(EngineKind::Vllm);
        with_boolean
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .push(EngineParameterDefinition {
                parameter_id: "feature-enabled".to_owned(),
                display_name: "Feature enabled".to_owned(),
                parameter_type: Some(ParameterType::Boolean(BooleanEngineParameterSchema {})),
                ..Default::default()
            });
        assert!(validate_engine_release(&with_boolean).is_empty());
        assert!(
            validate_engine_release(&release(EngineKind::LlamaCpp))
                .iter()
                .any(|error| error.to_string().contains("mismatched engine kind"))
        );
        let mut invalid = release(EngineKind::Vllm);
        invalid
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters[0]
            .parameter_type = None;
        assert!(
            validate_engine_release(&invalid)
                .iter()
                .any(|error| error.to_string().contains("non-empty supported"))
        );
    }

    #[test]
    fn rejects_shared_parameters_missing_or_above_vllm_limits() {
        for (parameter_id, maximum) in [
            (TOTAL_CONTEXT_PARAMETER_ID, Some(16_777_217)),
            (CONTEXT_PER_REQUEST_PARAMETER_ID, Some(1_048_577)),
            (PARALLEL_REQUESTS_PARAMETER_ID, Some(1025)),
            (PARALLEL_REQUESTS_PARAMETER_ID, None),
        ] {
            let mut release = release(EngineKind::Vllm);
            let parameters = &mut release
                .parameter_schema
                .as_mut()
                .expect("schema")
                .parameters;
            match maximum {
                Some(maximum) => {
                    if let Some(ParameterType::UnsignedInteger(range)) = parameters
                        .iter_mut()
                        .find(|definition| definition.parameter_id == parameter_id)
                        .and_then(|definition| definition.parameter_type.as_mut())
                    {
                        range.maximum = maximum;
                    }
                }
                None => parameters.retain(|definition| definition.parameter_id != parameter_id),
            }

            assert_eq!(
                validate_engine_release(&release)
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>(),
                [
                    "vLLM engine release must declare total-context up to 16777216 tokens, context-per-request up to 1048576 tokens and parallel-requests up to 1024"
                ],
                "{parameter_id} {maximum:?}"
            );
        }
    }

    #[test]
    fn rejects_unsupported_references_parallel_shape_and_readiness() {
        let configuration = ModelEngineConfiguration {
            configuration_id: "synthetic-invalid".to_owned(),
            engine_configuration: Some(EngineModelConfiguration {
                engine_kind: EngineKind::Vllm as i32,
                configuration: Some(Configuration::Vllm(VllmModelConfiguration {
                    fixed_settings: Some(VllmFixedModelSettings::default()),
                    ..Default::default()
                })),
            }),
            execution_capabilities: Some(ModelExecutionCapabilities::default()),
            ..Default::default()
        };
        let errors = validate_model_configuration(
            &Model::default(),
            &configuration,
            &release(EngineKind::Vllm),
        );
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("invalid fixed vLLM settings"))
        );
        assert!(errors.iter().any(|error| {
            error
                .to_string()
                .contains("invalid vLLM engine parameter references")
        }));
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("only one-target vLLM execution"))
        );
    }

    #[test]
    fn hardware_classes_must_select_only_nvidia() {
        let configuration = ModelEngineConfiguration {
            configuration_id: "synthetic-config".to_owned(),
            ..Default::default()
        };
        let nvidia = vendor_hardware_class("nvidia");
        let apple = vendor_hardware_class("apple");
        let mut both = vendor_hardware_class("both");
        both.selector.as_mut().expect("selector").accelerators =
            vec!["nvidia".to_owned(), "apple".to_owned()];

        assert!(validate_hardware_support(&configuration, &[&nvidia]).is_empty());
        assert_eq!(
            validate_hardware_support(&configuration, &[&apple, &nvidia, &both])
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            [
                "configuration \"synthetic-config\" hardware class \"apple\" must select only NVIDIA accelerators for vLLM",
                "configuration \"synthetic-config\" hardware class \"both\" must select only NVIDIA accelerators for vLLM",
            ]
        );
    }

    #[test]
    fn mtp_realization_requires_an_mtp_speculative_option() {
        let valid = |method: SpeculativeDecodingMethod| {
            valid_mtp_realization(
                &Model::default(),
                &ModelEngineConfiguration::default(),
                method,
                embedded_draft(),
            )
        };

        assert!(valid(SpeculativeDecodingMethod::Mtp));
        assert!(!valid(SpeculativeDecodingMethod::Unspecified));
    }

    #[test]
    fn rejects_a_kv_cache_precision_option() {
        let (model, configuration) = kv_cache_precision_declaration();
        let mut errors = Vec::new();

        validate_realizations(
            &model,
            &configuration,
            &VllmModelConfiguration::default(),
            &mut errors,
        );

        assert_eq!(
            errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "configuration \"local\" option \"kv-cache\" is a KV cache precision option, which vLLM does not support",
                "configuration \"local\" requires one unambiguous vLLM realization per model option",
            ]
        );
    }

    #[test]
    fn mtp_draft_is_embedded_or_an_unbound_snapshot_auxiliary_artifact() {
        let model = Model {
            auxiliary_artifacts: vec![
                auxiliary_artifact("draft", HuggingFaceContent::Snapshot(Default::default())),
                auxiliary_artifact("draft-file", HuggingFaceContent::File(Default::default())),
            ],
            ..Default::default()
        };
        let bound = |auxiliary_artifact_ids: &[&str]| ModelEngineConfiguration {
            artifact_bindings: vec![ModelArtifactBinding {
                auxiliary_artifact_ids: auxiliary_artifact_ids
                    .iter()
                    .map(|id| (*id).to_owned())
                    .collect(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let valid = |configuration: &ModelEngineConfiguration,
                     draft: Option<VllmMtpDraftSource>| {
            valid_mtp_realization(&model, configuration, SpeculativeDecodingMethod::Mtp, draft)
        };

        assert!(valid(&bound(&["template"]), embedded_draft()));
        assert!(valid(&bound(&["template"]), artifact_draft("draft")));
        assert!(!valid(
            &bound(&["template", "draft"]),
            artifact_draft("draft")
        ));
        assert!(!valid(&bound(&["template"]), artifact_draft("missing")));
        assert!(!valid(&bound(&["template"]), artifact_draft("draft-file")));
        assert!(!valid(&bound(&["template"]), artifact_draft("")));
        assert!(!valid(&bound(&["template"]), None));
        assert!(!valid(
            &bound(&["template"]),
            Some(VllmMtpDraftSource::default())
        ));
    }

    #[test]
    fn rejects_mismatched_payload_wrapper_and_release_kinds() {
        let configuration = ModelEngineConfiguration {
            configuration_id: "synthetic-config".to_owned(),
            engine_configuration: Some(EngineModelConfiguration {
                engine_kind: EngineKind::LlamaCpp as i32,
                configuration: Some(Configuration::LlamaCpp(Default::default())),
            }),
            ..Default::default()
        };
        let errors = validate_model_configuration(
            &Model::default(),
            &configuration,
            &release(EngineKind::Vllm),
        );
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("mismatched engine wrapper"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.to_string().contains("does not match vLLM"))
        );
    }

    #[test]
    fn rejects_parameter_references_to_shared_parameters() {
        let catalog = profile_shaped_catalog();
        let model = catalog
            .list_model_releases()
            .into_iter()
            .find(|model| model.model_id == "fixture-vllm-model")
            .expect("vLLM model");
        let engine = &catalog.list_engine_releases()[0];
        let mut configuration = model.configurations[0].clone();
        assert!(validate_model_configuration(&model, &configuration, engine).is_empty());

        match configuration
            .engine_configuration
            .as_mut()
            .and_then(|wrapper| wrapper.configuration.as_mut())
        {
            Some(Configuration::Vllm(payload)) => {
                payload
                    .fixed_settings
                    .as_mut()
                    .expect("fixed settings")
                    .maximum_batched_tokens_parameter_id =
                    PARALLEL_REQUESTS_PARAMETER_ID.to_owned();
            }
            _ => panic!("vLLM configuration"),
        }

        assert_eq!(
            validate_model_configuration(&model, &configuration, engine)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            ["configuration \"vllm-nvidia\" has invalid vLLM engine parameter references"]
        );
    }

    #[test]
    fn engine_defaults_launch_one_total_context_shared_by_four_requests() {
        let catalog = profile_shaped_catalog();

        let launch = launch(&catalog, "text", &[]).expect("vLLM launch at engine defaults");

        assert_eq!(
            launch_argv(&launch),
            [
                "vllm",
                "serve",
                "--host",
                "0.0.0.0",
                "--port",
                "15000",
                "--enable-chunked-prefill",
                "--enable-prefix-caching",
                "--language-model-only",
                "--served-model-name",
                "fixture-vllm-model",
                "--tool-call-parser",
                "qwen3_xml",
                "--tool-strict-level",
                "parameter",
                "--reasoning-parser",
                "qwen3",
                "--chat-template",
                "--max-model-len",
                "32768",
                "--max-num-seqs",
                "4",
                "--max-num-batched-tokens",
                "4096",
                "--max-cudagraph-capture-size",
                "32",
                "--kv-cache-dtype",
                "fp8",
                "--kv-cache-memory-bytes",
                "2563768320",
                "--gpu-memory-utilization",
                "0.6600",
            ]
        );
        // 51380224 × (1 + 4 × 7) + 32768 × 32768 bytes of KV cache: the 32768-token total context
        // plus the 4 requests' own blocks.
        assert_eq!(
            launch.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(22_563_768_320),
                derived_engine_parameter_ids: vec![
                    CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
                    "kv-cache-memory-bytes".to_owned(),
                ],
                host_memory_bytes: None,
            })
        );
    }

    #[test]
    fn the_memory_utilization_divides_by_the_gpu_total_of_its_own_target() {
        const RTX_4090_TOTAL: u64 = 24 * 1024 * 1024 * 1024;
        const RTX_4080_TOTAL: u64 = 16 * 1024 * 1024 * 1024;
        let catalog = profile_shaped_catalog();
        let snapshot = dotcore_node_hardware::MachineDescription::simulated(
            &["rtx-4090".to_owned(), "rtx-4080".to_owned()],
            "amd64",
        )
        .expect("simulated machine")
        .render(&[], std::time::SystemTime::now())
        .expect("rendered snapshot");
        let launch_on = |devices: &[&str; 1]| {
            let (mut target, _) = crate::test_support::execution_target_capabilities(
                ExecutionPlatform::linux_amd64(),
            );
            target.accelerator_device_ids = devices.iter().map(|id| (*id).to_owned()).collect();
            let inventory = TargetCapabilityReconciler::reconcile(
                [&target],
                [RuntimePlatformObservation::available(
                    &target.execution_target_id,
                    ExecutionPlatform::linux_amd64(),
                )],
                [NodeAcceleratorObservation::from_hardware_snapshot(
                    &target.site_node_id,
                    &snapshot,
                    false,
                )],
                dotcore_domain::timing::Timing::production()
                    .node_control
                    .capability_max_age,
            );
            let model_id = CatalogModelId("fixture-vllm-model".to_owned());
            let configuration = apply_profile_defaults(
                &catalog,
                &model_id,
                "text",
                &LocalModelDeploymentConfiguration {
                    model_artifact_binding_id: "snapshot".to_owned(),
                    ..Default::default()
                },
            )
            .expect("profile defaults");
            let launch = resolve_local_launch(
                &catalog,
                &model_id,
                "text",
                &configuration,
                &target,
                &inventory,
                None,
                std::time::SystemTime::now(),
            )
            .expect("vLLM launch on a GPU of a mixed machine");
            let total = launch
                .selected_target
                .as_ref()
                .and_then(|target| target.accelerator_memory_pool_total_bytes);
            let argv = launch_argv(&launch);
            let utilization = argv
                .windows(2)
                .find(|pair| pair[0] == "--gpu-memory-utilization")
                .map(|pair| pair[1].to_owned());
            (total, utilization)
        };

        // 22563768320 bytes over the 24 GiB GPU; over the 16 GiB one, it is capped at the whole
        // GPU.
        assert_eq!(
            launch_on(&["nvidia.com/gpu=0"]),
            (Some(RTX_4090_TOTAL), Some("0.8756".to_owned()))
        );
        assert_eq!(
            launch_on(&["nvidia.com/gpu=1"]),
            (Some(RTX_4080_TOTAL), Some("1.0000".to_owned()))
        );
    }

    #[test]
    fn engine_defaults_with_mtp_produce_the_exact_arguments() {
        let catalog = profile_shaped_catalog();

        let launch = launch(&catalog, "mtp", &[]).expect("vLLM MTP launch at engine defaults");

        // 57376768 × (1 + 4 × (7 + 3 × 3)) + 34816 × 32768 bytes of KV cache, plus the
        // 600000000-byte MTP head.
        assert_eq!(
            launch_argv(&launch),
            [
                "vllm",
                "serve",
                "--host",
                "0.0.0.0",
                "--port",
                "15000",
                "--enable-chunked-prefill",
                "--enable-prefix-caching",
                "--language-model-only",
                "--served-model-name",
                "fixture-vllm-model",
                "--speculative-config",
                r#"{"method":"mtp","num_speculative_tokens":3}"#,
                "--tool-call-parser",
                "qwen3_xml",
                "--tool-strict-level",
                "parameter",
                "--reasoning-parser",
                "qwen3",
                "--chat-template",
                "--max-model-len",
                "32768",
                "--max-num-seqs",
                "4",
                "--max-num-batched-tokens",
                "4096",
                "--max-cudagraph-capture-size",
                "32",
                "--kv-cache-dtype",
                "fp8",
                "--kv-cache-memory-bytes",
                "4870340608",
                "--gpu-memory-utilization",
                "0.7450",
            ]
        );
        assert_eq!(
            launch
                .sizing
                .as_ref()
                .and_then(|sizing| sizing.accelerator_memory_bytes),
            Some(25_470_340_608)
        );
    }

    #[test]
    fn a_larger_total_context_grows_only_the_kv_cache_memory() {
        let catalog = profile_shaped_catalog();

        let launch = launch(
            &catalog,
            "text",
            &[
                (TOTAL_CONTEXT_PARAMETER_ID, 131_072),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 65_536),
            ],
        )
        .expect("vLLM launch with a larger total context");

        let argv = launch_argv(&launch);
        // 51380224 × (1 + 4 × 7) + 32768 × 131072 bytes of KV cache.
        for (flag, value) in [
            ("--max-model-len", "65536"),
            ("--max-num-seqs", "4"),
            ("--kv-cache-memory-bytes", "5784993792"),
        ] {
            assert!(
                argv.windows(2).any(|pair| pair == [flag, value]),
                "{flag} {value}: {argv:?}"
            );
        }
    }

    #[test]
    fn more_parallel_requests_grow_only_the_kv_cache_memory() {
        let catalog = profile_shaped_catalog();

        let four = launch(&catalog, "text", &[]).expect("vLLM launch at 4 parallel requests");
        let eight = launch(&catalog, "text", &[(PARALLEL_REQUESTS_PARAMETER_ID, 8)])
            .expect("vLLM launch at 8 parallel requests");

        // Same total context: each added request adds its own 51380224 × 7 bytes of blocks.
        let changed: Vec<_> = launch_argv(&four)
            .windows(2)
            .zip(launch_argv(&eight).windows(2))
            .filter(|(before, after)| before[0].starts_with("--") && before[1] != after[1])
            .map(|(before, after)| (before[0], before[1], after[1]))
            .collect();
        assert_eq!(
            changed,
            [
                ("--max-num-seqs", "4", "8"),
                ("--kv-cache-memory-bytes", "2563768320", "4002414592"),
                ("--gpu-memory-utilization", "0.6600", "0.7021"),
            ]
        );
        let footprint = |launch: &ResolvedLocalModelDeploymentSnapshot| {
            launch
                .sizing
                .as_ref()
                .and_then(|sizing| sizing.accelerator_memory_bytes)
        };
        assert_eq!(
            (footprint(&four), footprint(&eight)),
            (Some(22_563_768_320), Some(24_002_414_592))
        );
    }

    #[test]
    fn rejects_a_context_per_request_above_the_total_context() {
        let catalog = profile_shaped_catalog();

        assert_eq!(
            launch(
                &catalog,
                "text",
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
engineReleaseId: "fixture-vllm"
engineKind: "ENGINE_KIND_VLLM"
version: "1"
parameterSchema:
  schemaId: "fixture-vllm-v2"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, unsignedInteger: { minimum: "1024", maximum: "16777216", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, unsignedInteger: { minimum: "1024", maximum: "1048576", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, unsignedInteger: { minimum: "1", maximum: "1024", unit: "requests", defaultValue: "4" } }
    - { parameterId: "maximum-batched-tokens", displayName: "Maximum batched tokens", unsignedInteger: { minimum: "1", maximum: "1048576", unit: "tokens" } }
    - { parameterId: "maximum-cudagraph-capture-size", displayName: "Maximum CUDA graph capture size", unsignedInteger: { minimum: "1", maximum: "1024", unit: "sequences" } }
    - { parameterId: "kv-cache-memory-bytes", displayName: "KV cache memory", unsignedInteger: { minimum: "1", maximum: "1099511627776", unit: "bytes" } }
    - { parameterId: "kv-offloading-size-gib", displayName: "KV offloading size", exposed: true, unsignedInteger: { minimum: "0", maximum: "1024", unit: "GiB" } }
artifacts:
  - artifactId: "linux-amd64-cuda"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "vllm" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

    const CATALOG_MODEL: &str = r#"
modelId: "fixture-vllm-model"
displayName: "Fixture vLLM model"
version: "1"
maximumContextTokens: 262144
options:
  - { optionId: "mtp", displayName: "MTP", speculativeDecoding: { method: "SPECULATIVE_DECODING_METHOD_MTP" } }
artifacts:
  - artifactId: "snapshot"
    source:
      repository: "dotcore/fixture-vllm"
      revision: "0123456789abcdef0123456789abcdef01234567"
      snapshot:
        files:
          - { path: "config.json", provenanceDigest: "1111111111111111111111111111111111111111111111111111111111111111" }
          - { path: "model.safetensors", provenanceDigest: "2222222222222222222222222222222222222222222222222222222222222222" }
auxiliaryArtifacts:
  - auxiliaryArtifactId: "chat-template"
    source:
      repository: "dotcore/fixture-vllm"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file: { path: "chat_template.jinja", provenanceDigest: "3333333333333333333333333333333333333333333333333333333333333333" }
configurations:
  - configurationId: "vllm-nvidia"
    engineArtifacts: [{ engineReleaseId: "fixture-vllm", engineArtifactId: "linux-amd64-cuda" }]
    artifactBindings: [{ bindingId: "snapshot", modelArtifactId: "snapshot", auxiliaryArtifactIds: ["chat-template"] }]
    sizing:
      parameters: { baseBytes: 20000000000, mtpBytes: 600000000, kvBlockBytes: 51380224, kvBytesPerToken: 32768, kvMtpBlockBytes: 57376768, kvMtpBytesPerToken: 34816 }
      derivedParameters:
        kv-cache-memory-bytes: >-
          if(${options.mtp.token-count} > 0, 1, 0) * (${kvMtpBlockBytes} * (1 + ${parallel-requests} * (7 + 3 * ${options.mtp.token-count})) + ${kvMtpBytesPerToken} * ${total-context})
          + if(${options.mtp.token-count} == 0, 1, 0) * (${kvBlockBytes} * (1 + ${parallel-requests} * 7) + ${kvBytesPerToken} * ${total-context})
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: >-
            ${baseBytes} + if(${options.mtp.token-count} > 0, 1, 0) * ${mtpBytes} + ${kv-cache-memory-bytes}
    hardwareSupport:
      - hardwareClassId: "nvidia"
        bindingSizing: [{ bindingId: "snapshot" }]
    engineConfiguration:
      engineKind: "ENGINE_KIND_VLLM"
      vllm:
        fixedSettings:
          reasoningParser: "qwen3"
          toolCallParser: "qwen3_xml"
          chatTemplateAuxiliaryArtifactId: "chat-template"
          kvCacheDataType: "VLLM_KV_CACHE_DATA_TYPE_FP8"
          kvCacheMemoryBytesParameterId: "kv-cache-memory-bytes"
          maximumBatchedTokensParameterId: "maximum-batched-tokens"
          maximumCudaGraphCaptureSizeParameterId: "maximum-cudagraph-capture-size"
          kvOffloadingSizeGibParameterId: "kv-offloading-size-gib"
          kvOffloadingBackend: "VLLM_KV_OFFLOADING_BACKEND_NATIVE"
          containerPort: 15000
          readinessPolicy: { mode: "VLLM_READINESS_MODE_MODELS" }
          enableChunkedPrefill: true
          enablePrefixCaching: true
        optionRealizations:
          - optionId: "mtp"
            mtp: { draft: { embedded: {} } }
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities: { supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"] }
    options:
      - optionId: "mtp"
        parameters:
          - { parameterId: "token-count", displayName: "Token count", unsignedInteger: { minimum: "0", maximum: "8", unit: "tokens" } }
        defaultValues:
          optionId: "mtp"
          values:
            - { parameterId: "token-count", unsignedInteger: "0" }
profiles:
  - profileId: "text"
    displayName: "Text"
    configurationId: "vllm-nvidia"
    defaultModelArtifactBindingId: "snapshot"
    engineParameterDefaults:
      values:
        - { parameterId: "maximum-batched-tokens", unsignedInteger: "4096" }
        - { parameterId: "maximum-cudagraph-capture-size", unsignedInteger: "32" }
        - { parameterId: "kv-offloading-size-gib", unsignedInteger: "0" }
  - profileId: "mtp"
    displayName: "MTP"
    configurationId: "vllm-nvidia"
    defaultModelArtifactBindingId: "snapshot"
    engineParameterDefaults:
      values:
        - { parameterId: "maximum-batched-tokens", unsignedInteger: "4096" }
        - { parameterId: "maximum-cudagraph-capture-size", unsignedInteger: "32" }
        - { parameterId: "kv-offloading-size-gib", unsignedInteger: "0" }
    optionDefaults:
      - optionId: "mtp"
        values:
          - { parameterId: "token-count", unsignedInteger: "3" }
"#;

    /// Loads the profile-shaped engine and model with the shared hardware classes.
    fn profile_shaped_catalog() -> dotcore_catalog::Catalog {
        let root = tempfile::tempdir().expect("catalog root");
        for (path, content) in [
            ("engines/vllm.yaml", CATALOG_ENGINE),
            ("models/vllm-model.yaml", CATALOG_MODEL),
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
        .expect("vLLM catalog")
    }

    /// Resolves the profile on a 34190917632-byte NVIDIA target with the supplied engine
    /// parameter values after the profile and engine defaults fill the rest, as the controller
    /// does.
    fn launch(
        catalog: &dotcore_catalog::Catalog,
        profile_id: &str,
        values: &[(&str, u64)],
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
        let model_id = CatalogModelId("fixture-vllm-model".to_owned());
        let configuration = apply_profile_defaults(
            catalog,
            &model_id,
            profile_id,
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "snapshot".to_owned(),
                engine_parameter_values: Some(unsigned_parameter_values(values.iter().copied())),
                ..Default::default()
            },
        )?;
        let (target, inventory) =
            crate::test_support::execution_target_capabilities(ExecutionPlatform::linux_amd64());

        resolve_local_launch(
            catalog,
            &model_id,
            profile_id,
            &configuration,
            &target,
            &inventory,
            None,
            std::time::SystemTime::now(),
        )
    }

    /// Literal arguments of the launch plan, without the artifact bindings.
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

    fn valid_mtp_realization(
        model: &Model,
        configuration: &ModelEngineConfiguration,
        method: SpeculativeDecodingMethod,
        draft: Option<VllmMtpDraftSource>,
    ) -> bool {
        valid_realization(
            model,
            configuration,
            Some(&Realization::Mtp(VllmMtpOptionRealization { draft })),
            &ModelConfigurationOption::default(),
            Some(&ModelOptionKind::SpeculativeDecoding(
                SpeculativeDecodingModelOption {
                    method: method as i32,
                },
            )),
            &mut 0,
            &mut 0,
        )
    }

    /// Model and configuration declaring one KV cache precision option and no realization.
    fn kv_cache_precision_declaration() -> (Model, ModelEngineConfiguration) {
        let model = Model {
            options: vec![dotcore_proto::dotcore::catalog::v1::ModelOption {
                option_id: "kv-cache".to_owned(),
                display_name: "KV cache precision".to_owned(),
                kind: Some(ModelOptionKind::KvCachePrecision(
                    dotcore_proto::dotcore::catalog::v1::KvCachePrecisionModelOption {},
                )),
            }],
            ..Default::default()
        };
        let configuration = ModelEngineConfiguration {
            configuration_id: "local".to_owned(),
            options: vec![ModelConfigurationOption {
                option_id: "kv-cache".to_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };
        (model, configuration)
    }

    fn embedded_draft() -> Option<VllmMtpDraftSource> {
        Some(VllmMtpDraftSource {
            source: Some(Source::Embedded(VllmEmbeddedMtpHead {})),
        })
    }

    fn artifact_draft(artifact_id: &str) -> Option<VllmMtpDraftSource> {
        Some(VllmMtpDraftSource {
            source: Some(Source::AuxiliaryArtifactId(artifact_id.to_owned())),
        })
    }

    fn auxiliary_artifact(
        artifact_id: &str,
        content: HuggingFaceContent,
    ) -> ModelAuxiliaryArtifact {
        ModelAuxiliaryArtifact {
            auxiliary_artifact_id: artifact_id.to_owned(),
            source: Some(HuggingFaceArtifactSource {
                content: Some(content),
                ..Default::default()
            }),
        }
    }
}
