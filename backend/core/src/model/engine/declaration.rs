//! Engine declaration validation dispatch.

use dotcore_catalog::{
    EngineDeclarationValidator,
    shared_parameters::{CONTEXT_PER_REQUEST_PARAMETER_ID, MINIMUM_CONTEXT_TOKENS},
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::catalog::v1::{
    EngineParameterValues, EngineRelease, HardwareClass, InferenceRequestKind, Model,
    ModelEngineConfiguration, model_option,
};

use super::definition::{definition, unsigned_parameter_maximum};

/// Static inference-owned dispatcher for engine declaration semantics.
#[derive(Debug, Default)]
pub struct EngineFacade;

impl EngineDeclarationValidator for EngineFacade {
    fn validate_engine_release(&self, engine: &EngineRelease) -> Vec<anyhow::Error> {
        match definition(engine.engine_kind) {
            Some(definition) => definition.validate_engine_release(engine),
            None => vec![anyhow::anyhow!(
                "engine release has an unsupported engine kind"
            )],
        }
    }

    fn validate_model_configuration(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        hardware_classes: &[&HardwareClass],
        engine: &EngineRelease,
    ) -> Vec<anyhow::Error> {
        match definition(engine.engine_kind) {
            Some(definition) => {
                let mut errors =
                    definition.validate_model_configuration(model, configuration, engine);
                errors
                    .extend(definition.validate_hardware_support(configuration, hardware_classes));
                errors.extend(configuration_maximum_context_problems(
                    model,
                    configuration,
                    engine,
                ));
                errors.extend(unsupported_request_kinds(
                    definition.name(),
                    configuration,
                    definition.request_kinds(),
                ));
                errors
            }
            None => vec![anyhow::anyhow!(
                "configuration {:?} uses an unsupported engine kind",
                configuration.configuration_id
            )],
        }
    }

    fn launch_requires_sizing(&self, engine: &EngineRelease) -> bool {
        definition(engine.engine_kind).is_some_and(|definition| definition.launch_requires_sizing())
    }

    fn configuration_parameter_defaults(
        &self,
        model: &Model,
        configuration: &ModelEngineConfiguration,
        engine: &EngineRelease,
    ) -> EngineParameterValues {
        definition(engine.engine_kind)
            .map(|definition| definition.parameter_defaults(model, configuration))
            .unwrap_or_default()
    }
}

/// Returns an error when the configuration declares a request kind outside `allowed`, the kinds
/// its engine serves.
fn unsupported_request_kinds(
    engine_name: &str,
    configuration: &ModelEngineConfiguration,
    allowed: &[InferenceRequestKind],
) -> Option<anyhow::Error> {
    configuration
        .request_kinds
        .iter()
        .any(|value| {
            InferenceRequestKind::try_from(*value).is_ok_and(|kind| !allowed.contains(&kind))
        })
        .then(|| {
            anyhow::anyhow!(
                "configuration {:?} declares a request kind {engine_name} does not serve",
                configuration.configuration_id
            )
        })
}

/// Returns the ids of the hardware classes whose selector names no accelerator, or one outside
/// `vendors`, in the order given.
pub(crate) fn classes_outside_vendors<'a>(
    hardware_classes: &[&'a HardwareClass],
    vendors: &[Accelerator],
) -> Vec<&'a str> {
    hardware_classes
        .iter()
        .filter(|hardware_class| {
            let accelerators = hardware_class
                .selector
                .as_ref()
                .map(|selector| selector.accelerators.as_slice())
                .unwrap_or_default();
            accelerators.is_empty()
                || accelerators.iter().any(|name| {
                    Accelerator::from_catalog_name(name)
                        .is_none_or(|vendor| !vendors.contains(&vendor))
                })
        })
        .map(|hardware_class| hardware_class.hardware_class_id.as_str())
        .collect()
}

/// Returns the ids of the configuration's options whose model option is a KV cache precision
/// option, in declaration order, for engines that do not realize the kind.
pub(crate) fn kv_cache_precision_option_ids<'a>(
    model: &Model,
    configuration: &'a ModelEngineConfiguration,
) -> Vec<&'a str> {
    configuration
        .options
        .iter()
        .filter(|option| {
            model.options.iter().any(|candidate| {
                candidate.option_id == option.option_id
                    && matches!(
                        candidate.kind,
                        Some(model_option::Kind::KvCachePrecision(_))
                    )
            })
        })
        .map(|option| option.option_id.as_str())
        .collect()
}

/// Rejects a configuration maximum context below the minimum context, above the model's maximum
/// context, which a configuration may only lower, or above the context per request its engine
/// release accepts. Catalog validation reports a model without a maximum on its own.
fn configuration_maximum_context_problems(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
) -> Vec<anyhow::Error> {
    let Some(maximum) = configuration.maximum_context_tokens else {
        return Vec::new();
    };
    if u64::from(maximum) < MINIMUM_CONTEXT_TOKENS {
        return vec![anyhow::anyhow!(
            "configuration {:?} maximum context of {maximum} tokens is below the minimum context of {MINIMUM_CONTEXT_TOKENS} tokens",
            configuration.configuration_id
        )];
    }
    if model.maximum_context_tokens != 0 && maximum > model.maximum_context_tokens {
        return vec![anyhow::anyhow!(
            "configuration {:?} maximum context of {maximum} tokens is above the model's maximum context of {} tokens; a configuration may only lower it",
            configuration.configuration_id,
            model.maximum_context_tokens
        )];
    }
    if let Some(release_maximum) =
        unsigned_parameter_maximum(engine, CONTEXT_PER_REQUEST_PARAMETER_ID)
        && u64::from(maximum) > release_maximum
    {
        return vec![anyhow::anyhow!(
            "configuration {:?} maximum context of {maximum} tokens is above the context per request of up to {release_maximum} tokens its engine release accepts; lower the configuration maximum",
            configuration.configuration_id
        )];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::{
        EngineDeclarationValidator, shared_parameters::CONTEXT_PER_REQUEST_PARAMETER_ID,
        test_support::vendor_hardware_class,
    };
    use dotcore_proto::dotcore::{
        catalog::v1::{
            EngineParameterDefinition, EngineParameterSchema, EngineRelease, InferenceRequestKind,
            Model, ModelEngineConfiguration, UnsignedIntegerEngineParameterSchema,
            engine_parameter_definition::ParameterType,
        },
        engine::v1::{EngineKind, EngineModelConfiguration},
    };

    use super::{EngineFacade, configuration_maximum_context_problems, unsupported_request_kinds};

    #[test]
    fn a_configuration_maximum_context_stays_within_the_minimum_the_model_and_the_release() {
        let model = Model {
            maximum_context_tokens: 262_144,
            ..Default::default()
        };
        let release = EngineRelease {
            parameter_schema: Some(EngineParameterSchema {
                parameters: vec![EngineParameterDefinition {
                    parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
                    parameter_type: Some(ParameterType::UnsignedInteger(
                        UnsignedIntegerEngineParameterSchema {
                            minimum: 1024,
                            maximum: 131_072,
                            unit: "tokens".to_owned(),
                            default_value: None,
                        },
                    )),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let problems = |maximum_context_tokens: Option<u32>| {
            let configuration = ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                maximum_context_tokens,
                ..Default::default()
            };
            configuration_maximum_context_problems(&model, &configuration, &release)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        };

        for accepted in [None, Some(1024), Some(65_536), Some(131_072)] {
            assert_eq!(problems(accepted), Vec::<String>::new(), "{accepted:?}");
        }
        for (maximum, expected) in [
            (
                0,
                "configuration \"local\" maximum context of 0 tokens is below the minimum context of 1024 tokens",
            ),
            (
                1023,
                "configuration \"local\" maximum context of 1023 tokens is below the minimum context of 1024 tokens",
            ),
            (
                131_073,
                "configuration \"local\" maximum context of 131073 tokens is above the context per request of up to 131072 tokens its engine release accepts; lower the configuration maximum",
            ),
            (
                262_145,
                "configuration \"local\" maximum context of 262145 tokens is above the model's maximum context of 262144 tokens; a configuration may only lower it",
            ),
        ] {
            assert_eq!(problems(Some(maximum)), [expected], "{maximum}");
        }
    }

    #[test]
    fn engines_reject_request_kinds_they_do_not_serve() {
        let configuration = |kinds: &[InferenceRequestKind]| ModelEngineConfiguration {
            configuration_id: "local".to_owned(),
            request_kinds: kinds.iter().map(|kind| *kind as i32).collect(),
            ..Default::default()
        };
        let chat_only = [InferenceRequestKind::Chat];
        let both = [InferenceRequestKind::Chat, InferenceRequestKind::Decision];

        assert!(
            unsupported_request_kinds("vLLM", &configuration(&chat_only), &chat_only).is_none()
        );
        assert!(unsupported_request_kinds("llama.cpp", &configuration(&both), &both).is_none());
        assert_eq!(
            unsupported_request_kinds("vLLM", &configuration(&both), &chat_only)
                .expect("decision rejected")
                .to_string(),
            "configuration \"local\" declares a request kind vLLM does not serve"
        );
    }

    #[test]
    fn only_vllm_and_strata_launches_require_sizing() {
        for (engine_kind, expected) in [
            (EngineKind::Vllm, true),
            (EngineKind::LlamaCpp, false),
            (EngineKind::Ninfer, false),
            (EngineKind::Strata, true),
            (EngineKind::Unspecified, false),
        ] {
            let release = EngineRelease {
                engine_kind: engine_kind as i32,
                ..Default::default()
            };

            assert_eq!(
                EngineFacade.launch_requires_sizing(&release),
                expected,
                "{engine_kind:?}"
            );
        }
    }

    #[test]
    fn dispatcher_rejects_unspecified_engine_kind() {
        let errors = EngineFacade.validate_engine_release(&EngineRelease::default());

        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].to_string(),
            "engine release has an unsupported engine kind"
        );
    }

    #[test]
    fn dispatcher_checks_the_engine_vendors_of_supported_hardware_classes() {
        let configuration = ModelEngineConfiguration {
            configuration_id: "default".to_owned(),
            ..Default::default()
        };
        let release = EngineRelease {
            engine_kind: EngineKind::Vllm as i32,
            ..Default::default()
        };

        let errors = EngineFacade.validate_model_configuration(
            &Model::default(),
            &configuration,
            &[&vendor_hardware_class("apple")],
            &release,
        );

        assert!(
            errors.iter().any(|error| error.to_string()
                == "configuration \"default\" hardware class \"apple\" must select only NVIDIA accelerators for vLLM"),
            "{errors:?}"
        );
    }

    #[test]
    fn llama_cpp_rejects_missing_payload() {
        let model = Model::default();
        let configuration = ModelEngineConfiguration {
            configuration_id: "default".to_owned(),
            engine_configuration: Some(EngineModelConfiguration {
                engine_kind: EngineKind::LlamaCpp as i32,
                configuration: None,
            }),
            ..Default::default()
        };
        let release = EngineRelease {
            engine_kind: EngineKind::LlamaCpp as i32,
            ..Default::default()
        };

        let errors = EngineFacade.validate_model_configuration(
            &model,
            &configuration,
            &[&vendor_hardware_class("nvidia")],
            &release,
        );

        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].to_string(),
            "configuration \"default\" has a payload that does not match llama.cpp"
        );
    }
}
