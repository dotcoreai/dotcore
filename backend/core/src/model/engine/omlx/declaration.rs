//! oMLX-specific validation of catalog engine releases and model engine configurations.
//!
//! A release must declare well-formed unsigned-integer parameters and only native Nix programs
//! for darwin/arm64 with Apple accelerators, the one platform oMLX's release runs on. A
//! configuration must bind pinned MLX snapshots, run text-only on one Apple accelerator, declare no
//! options, and reference a host prompt cache memory parameter so that every release parameter it
//! keeps reaches the launch. Launch values must give oMLX a context per request that is the total
//! context split evenly among the parallel requests, since oMLX caps each request, not their sum.

use std::collections::BTreeSet;

use dotcore_catalog::shared_parameters::{
    CONTEXT_PER_REQUEST_PARAMETER_ID, ContextRule, PARALLEL_REQUESTS_PARAMETER_ID,
    TOTAL_CONTEXT_PARAMETER_ID, is_shared_parameter,
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineRelease, HardwareClass, MemoryPool, Model, ModelEngineConfiguration,
        ModelInputModality, ModelParallelShape, ParameterDefinition, ParameterValues,
        engine_artifact::Kind as EngineArtifactKind,
        hugging_face_artifact_source::Content as HuggingFaceContent, nix_artifact::Provides,
        parameter_definition::ParameterType,
    },
    engine::{
        omlx::v1::{OmlxFixedModelSettings, OmlxModelConfiguration},
        v1::{EngineKind, engine_model_configuration::Configuration},
    },
};

use crate::model::engine::{
    declaration::classes_outside_vendors, definition::unsigned_parameter_value,
};

/// File at a snapshot's root that oMLX discovers a model folder by.
const MODEL_CONFIG_FILE: &str = "config.json";

/// Extension of the MLX weight files oMLX loads.
const WEIGHTS_FILE_EXTENSION: &str = ".safetensors";

/// Unit of the prompt cache memory parameter, which the plan converts for the launcher.
const PROMPT_CACHE_UNIT: &str = "MiB";

/// oMLX's context rule: each parallel request may use the whole context per request, so a total
/// context fills the context per request as its even split among the parallel requests, capped by
/// the maximum context, and the derived total context is exactly the context per request times the
/// parallel requests, as the shared rule derives it.
pub(super) struct OmlxContextRule;

impl ContextRule for OmlxContextRule {
    fn filled_context_per_request(
        &self,
        total_context: u64,
        parallel_requests: u64,
        maximum_context_tokens: u64,
    ) -> u64 {
        (total_context / parallel_requests.max(1)).min(maximum_context_tokens)
    }
}

/// Rejects a context per request other than the total context split evenly among the parallel
/// requests, rounded down: oMLX caps each request at the context per request and admits that many
/// requests at once, so the total context sizing counts holds their product. Rounding down keeps
/// the accepted totals one unbroken range, which the largest-fit search relies on.
pub(crate) fn validate_parameter_values(values: &ParameterValues) -> anyhow::Result<()> {
    let (Some(total_context), Some(context_per_request), Some(parallel_requests)) = (
        unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID),
        unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID),
        unsigned_parameter_value(values, PARALLEL_REQUESTS_PARAMETER_ID),
    ) else {
        return Ok(());
    };
    let split = total_context / parallel_requests.max(1);
    if context_per_request != split {
        let product = context_per_request.saturating_mul(parallel_requests);
        anyhow::bail!(
            "context per request of {context_per_request} tokens is not the total context of {total_context} tokens split among {parallel_requests} parallel requests, {split} tokens each; oMLX lets each parallel request use its full context per request, so leave the total context out or set it to {product} tokens"
        );
    }
    Ok(())
}

/// Accepts a release of non-empty unsigned-integer parameters whose artifacts are all native Nix
/// programs selected for darwin/arm64 Apple targets. The catalog checks the shared parameters'
/// fixed shape, and each release declares oMLX's own bounds.
pub(crate) fn validate_engine_release(engine: &EngineRelease) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if engine.engine_kind != EngineKind::Omlx as i32 {
        errors.push(anyhow::anyhow!(
            "oMLX engine release has a mismatched engine kind"
        ));
    }
    let valid = engine.parameter_schema.as_ref().is_some_and(|schema| {
        !schema.parameters.is_empty()
            && schema.parameters.iter().all(|definition| {
                !definition.parameter_id.is_empty()
                    && matches!(
                        definition.parameter_type.as_ref(),
                        Some(ParameterType::UnsignedInteger(range)) if !range.unit.is_empty()
                    )
            })
    });
    if !valid {
        errors.push(anyhow::anyhow!(
            "oMLX engine release requires non-empty unsigned-integer parameter definitions"
        ));
    }
    let native_apple = !engine.artifacts.is_empty()
        && engine.artifacts.iter().all(|artifact| {
            let native = matches!(
                artifact.kind.as_ref(),
                Some(EngineArtifactKind::Nix(nix))
                    if matches!(nix.provides, Some(Provides::Executable(_)))
            );
            native
                && !artifact.selectors.is_empty()
                && artifact.selectors.iter().all(|selector| {
                    selector.accelerators == [Accelerator::Apple.catalog_name()]
                        && selector.platform.as_ref().is_some_and(|platform| {
                            platform.operating_system == "darwin"
                                && platform.architecture == "arm64"
                        })
                })
        });
    if !native_apple {
        errors.push(anyhow::anyhow!(
            "oMLX engine release supports only native Nix programs for darwin/arm64 with Apple accelerators"
        ));
    }
    errors
}

/// Accepts supported hardware classes that select only Apple accelerators. Selectors match
/// single-device targets only, so oMLX runs on exactly one Apple GPU.
pub(crate) fn validate_hardware_support(
    configuration: &ModelEngineConfiguration,
    hardware_classes: &[&HardwareClass],
) -> Vec<anyhow::Error> {
    classes_outside_vendors(hardware_classes, &[Accelerator::Apple])
        .into_iter()
        .map(|hardware_class_id| {
            anyhow::anyhow!(
                "configuration {:?} hardware class {hardware_class_id:?} must select only Apple accelerators for oMLX",
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
    if engine.engine_kind != EngineKind::Omlx as i32
        || configuration
            .engine_configuration
            .as_ref()
            .is_none_or(|wrapper| wrapper.engine_kind != EngineKind::Omlx as i32)
    {
        errors.push(anyhow::anyhow!(
            "oMLX configuration has a mismatched engine wrapper or release kind"
        ));
    }
    let Some(payload) = configuration.engine_configuration.as_ref() else {
        return errors;
    };
    let Some(Configuration::Omlx(payload)) = payload.configuration.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a payload that does not match oMLX",
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
    payload: &OmlxModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(fixed) = payload.fixed_settings.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires fixed oMLX settings",
            configuration.configuration_id
        ));
        return;
    };

    validate_parameters(configuration, engine, fixed, errors);
    validate_artifacts(model, configuration, errors);
    validate_execution_shape(configuration, errors);
    validate_inputs_and_options(configuration, errors);
}

fn validate_parameters(
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
    fixed: &OmlxFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let parameters = engine
        .parameter_schema
        .as_ref()
        .map(|schema| schema.parameters.as_slice())
        .unwrap_or_default();
    let prompt_cache = fixed.prompt_cache_mib_parameter_id.as_str();
    let valid_prompt_cache = !is_shared_parameter(prompt_cache)
        && parameters
            .iter()
            .find(|definition| definition.parameter_id == prompt_cache)
            .is_some_and(is_host_memory_mib);
    if !valid_prompt_cache {
        errors.push(anyhow::anyhow!(
            "configuration {:?} prompt cache parameter must name a release parameter in {PROMPT_CACHE_UNIT} drawn from host memory",
            configuration.configuration_id
        ));
    }

    // Every parameter of the configuration must reach the launch plan, so none can be set without
    // effect. The total context reaches it through the memory ceiling sizing derives from it.
    let declared = parameters
        .iter()
        .map(|definition| definition.parameter_id.as_str())
        .collect::<BTreeSet<_>>();
    let launched = BTreeSet::from([
        TOTAL_CONTEXT_PARAMETER_ID,
        CONTEXT_PER_REQUEST_PARAMETER_ID,
        PARALLEL_REQUESTS_PARAMETER_ID,
        prompt_cache,
    ]);
    if declared != launched {
        errors.push(anyhow::anyhow!(
            "configuration {:?} must keep exactly the shared context parameters and its prompt cache parameter",
            configuration.configuration_id
        ));
    }
}

/// Whether a parameter is an unsigned memory size in MiB drawn from the host pool.
fn is_host_memory_mib(definition: &ParameterDefinition) -> bool {
    matches!(
        definition.parameter_type.as_ref(),
        Some(ParameterType::UnsignedInteger(range))
            if range.unit == PROMPT_CACHE_UNIT && range.memory_pool == Some(MemoryPool::Host as i32)
    )
}

/// Accepts bindings of MLX snapshots: a model configuration and weight files at the snapshot's
/// root, which oMLX loads as one model folder, and no auxiliary artifacts.
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
                    .is_some_and(is_mlx_snapshot)
        });
    if !valid {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires snapshot model artifacts holding {MODEL_CONFIG_FILE} and {WEIGHTS_FILE_EXTENSION} weights at their root, without auxiliary artifacts",
            configuration.configuration_id
        ));
    }
}

fn is_mlx_snapshot(content: &HuggingFaceContent) -> bool {
    let HuggingFaceContent::Snapshot(snapshot) = content else {
        return false;
    };
    let root_files = || {
        snapshot
            .files
            .iter()
            .map(|file| file.path.as_str())
            .filter(|path| !path.contains('/'))
    };
    root_files().any(|path| path == MODEL_CONFIG_FILE)
        && root_files().any(|path| {
            path.len() > WEIGHTS_FILE_EXTENSION.len() && path.ends_with(WEIGHTS_FILE_EXTENSION)
        })
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
            "configuration {:?} supports only one-target oMLX execution",
            configuration.configuration_id
        ));
    }
}

/// The adapter launches oMLX with fixed settings and text input only, so a configuration has no
/// options to realize and no image input.
fn validate_inputs_and_options(
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    if !configuration.options.is_empty() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} declares options, which oMLX does not realize",
            configuration.configuration_id
        ));
    }
    let image_input = configuration
        .input_capabilities
        .as_ref()
        .is_some_and(|capabilities| {
            capabilities
                .supported_modalities
                .contains(&(ModelInputModality::Image as i32))
                || capabilities.image_input_capacity.is_some()
        });
    if image_input {
        errors.push(anyhow::anyhow!(
            "configuration {:?} declares image input, which oMLX does not serve",
            configuration.configuration_id
        ));
    }
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::test_support::vendor_hardware_class;
    use dotcore_proto::dotcore::catalog::v1::{
        BooleanParameterSchema, EngineArtifact, HuggingFaceArtifactSource, HuggingFaceFileArtifact,
        ModelConfigurationOption, ModelImageInputCapacity, NixArtifact, NixDockerImage,
        UnsignedIntegerParameterSchema,
    };

    use super::*;
    use crate::test_support::{
        omlx::{PROMPT_CACHE_MIB_PARAMETER_ID, configuration, model, release},
        unsigned_parameter_values,
    };

    #[test]
    fn accepts_the_synthetic_release_and_configuration() {
        assert_eq!(
            messages(validate_engine_release(&release())),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &configuration(),
                &release()
            )),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(validate_hardware_support(
                &configuration(),
                &[&vendor_hardware_class("apple")]
            )),
            Vec::<String>::new()
        );
    }

    #[test]
    fn rejects_release_with_wrong_kind_or_unsupported_parameter_schema() {
        let mut wrong_kind = release();
        wrong_kind.engine_kind = EngineKind::LlamaCpp as i32;
        assert_eq!(
            messages(validate_engine_release(&wrong_kind)),
            ["oMLX engine release has a mismatched engine kind"]
        );

        let mut boolean = release();
        parameter(&mut boolean, PROMPT_CACHE_MIB_PARAMETER_ID).parameter_type =
            Some(ParameterType::Boolean(BooleanParameterSchema::default()));
        assert_eq!(
            messages(validate_engine_release(&boolean)),
            ["oMLX engine release requires non-empty unsigned-integer parameter definitions"]
        );
    }

    #[test]
    fn rejects_release_artifacts_other_than_native_darwin_apple_programs() {
        let docker_image = |artifact: &mut EngineArtifact| {
            artifact.kind = Some(EngineArtifactKind::Nix(NixArtifact {
                flake: "self".to_owned(),
                package: Some("engines.omlx.docker".to_owned()),
                provides: Some(Provides::DockerImage(NixDockerImage {})),
                store_paths: Vec::new(),
            }));
        };
        let oci = |artifact: &mut EngineArtifact| {
            *artifact = dotcore_domain::test_support::oci::engine_artifact(
                "darwin-arm64",
                Some("registry.dotcore.test"),
                "omlx",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "darwin",
                "arm64",
                "apple",
            );
        };
        let linux = |artifact: &mut EngineArtifact| {
            artifact.selectors[0]
                .platform
                .as_mut()
                .expect("platform")
                .operating_system = "linux".to_owned();
        };
        let nvidia = |artifact: &mut EngineArtifact| {
            artifact.selectors[0].accelerators = vec!["nvidia".to_owned()];
        };
        let no_selector = |artifact: &mut EngineArtifact| artifact.selectors.clear();

        for (case, edit) in [
            (
                "Docker image",
                &docker_image as &dyn Fn(&mut EngineArtifact),
            ),
            ("OCI image", &oci),
            ("Linux", &linux),
            ("NVIDIA", &nvidia),
            ("no selector", &no_selector),
        ] {
            let mut release = release();
            edit(&mut release.artifacts[0]);

            assert_eq!(
                messages(validate_engine_release(&release)),
                [
                    "oMLX engine release supports only native Nix programs for darwin/arm64 with Apple accelerators"
                ],
                "{case}"
            );
        }

        let mut none = release();
        none.artifacts.clear();
        assert_eq!(
            messages(validate_engine_release(&none)),
            [
                "oMLX engine release supports only native Nix programs for darwin/arm64 with Apple accelerators"
            ]
        );
    }

    #[test]
    fn rejects_hardware_classes_beyond_apple() {
        assert_eq!(
            messages(validate_hardware_support(
                &configuration(),
                &[
                    &vendor_hardware_class("apple"),
                    &vendor_hardware_class("nvidia")
                ]
            )),
            [
                "configuration \"omlx-apple\" hardware class \"nvidia\" must select only Apple accelerators for oMLX"
            ]
        );
    }

    #[test]
    fn rejects_a_mismatched_or_incomplete_payload() {
        let mut no_fixed = configuration();
        payload(&mut no_fixed).fixed_settings = None;
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &no_fixed,
                &release()
            )),
            ["configuration \"omlx-apple\" requires fixed oMLX settings"]
        );

        let mut other_engine = configuration();
        other_engine.engine_configuration = Some(
            dotcore_proto::dotcore::engine::v1::EngineModelConfiguration {
                engine_kind: EngineKind::Omlx as i32,
                configuration: Some(Configuration::Strata(Default::default())),
            },
        );
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &other_engine,
                &release()
            )),
            ["configuration \"omlx-apple\" has a payload that does not match oMLX"]
        );
    }

    #[test]
    fn rejects_a_prompt_cache_reference_that_is_missing_shared_or_not_host_mib() {
        let invalid = "configuration \"omlx-apple\" prompt cache parameter must name a release parameter in MiB drawn from host memory";
        let not_kept = "configuration \"omlx-apple\" must keep exactly the shared context parameters and its prompt cache parameter";
        for reference in ["missing", TOTAL_CONTEXT_PARAMETER_ID, ""] {
            let mut configuration = configuration();
            fixed(&mut configuration).prompt_cache_mib_parameter_id = reference.to_owned();

            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration,
                    &release()
                )),
                [invalid, not_kept],
                "{reference:?}"
            );
        }

        for (case, unit, memory_pool) in [
            ("GiB", "GiB", Some(MemoryPool::Host as i32)),
            (
                "accelerator pool",
                "MiB",
                Some(MemoryPool::Accelerator as i32),
            ),
            ("no pool", "MiB", None),
        ] {
            let mut release = release();
            parameter(&mut release, PROMPT_CACHE_MIB_PARAMETER_ID).parameter_type = Some(
                ParameterType::UnsignedInteger(UnsignedIntegerParameterSchema {
                    minimum: 0,
                    maximum: None,
                    unit: unit.to_owned(),
                    default_value: None,
                    memory_pool,
                }),
            );

            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration(),
                    &release
                )),
                [invalid],
                "{case}"
            );
        }
    }

    #[test]
    fn rejects_release_parameters_that_would_not_reach_the_launch() {
        let mut extra = release();
        let mut kv_bits = parameter(&mut extra, PROMPT_CACHE_MIB_PARAMETER_ID).clone();
        kv_bits.parameter_id = "kv-bits".to_owned();
        extra
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .push(kv_bits);

        assert_eq!(
            messages(validate_engine_release(&extra)),
            Vec::<String>::new()
        );
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &configuration(),
                &extra
            )),
            [
                "configuration \"omlx-apple\" must keep exactly the shared context parameters and its prompt cache parameter"
            ]
        );
    }

    #[test]
    fn rejects_bindings_other_than_mlx_snapshots() {
        let message = "configuration \"omlx-apple\" requires snapshot model artifacts holding config.json and .safetensors weights at their root, without auxiliary artifacts";
        let mut gguf = model();
        gguf.artifacts[0].source = Some(HuggingFaceArtifactSource {
            repository: "dotcore/fixture".to_owned(),
            revision: "0123456789abcdef0123456789abcdef01234567".to_owned(),
            content: Some(HuggingFaceContent::File(HuggingFaceFileArtifact {
                path: "fixture-Q4_K_M.gguf".to_owned(),
                provenance_digest: "0".repeat(64),
            })),
        });
        let mut no_config = model();
        snapshot_files(&mut no_config).retain(|file| file.path != "config.json");
        let mut no_weights = model();
        snapshot_files(&mut no_weights).retain(|file| file.path != "model.safetensors");
        let mut nested = model();
        for file in snapshot_files(&mut nested) {
            file.path = format!("4bit/{}", file.path);
        }

        for (case, model) in [
            ("GGUF file", gguf),
            ("no config", no_config),
            ("no weights", no_weights),
            ("nested", nested),
        ] {
            assert_eq!(
                messages(validate_model_configuration(
                    &model,
                    &configuration(),
                    &release()
                )),
                [message],
                "{case}"
            );
        }

        let mut auxiliary = configuration();
        auxiliary.artifact_bindings[0].auxiliary_artifact_ids = vec!["draft".to_owned()];
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &auxiliary,
                &release()
            )),
            [message]
        );
    }

    #[test]
    fn rejects_execution_without_one_target_options_and_image_input() {
        let mut no_shape = configuration();
        no_shape
            .execution_capabilities
            .as_mut()
            .expect("execution capabilities")
            .supported_parallel_shapes
            .clear();
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &no_shape,
                &release()
            )),
            ["configuration \"omlx-apple\" supports only one-target oMLX execution"]
        );

        let mut options = configuration();
        options.options = vec![ModelConfigurationOption {
            option_id: "mtp".to_owned(),
            ..Default::default()
        }];
        assert_eq!(
            messages(validate_model_configuration(&model(), &options, &release())),
            ["configuration \"omlx-apple\" declares options, which oMLX does not realize"]
        );

        let mut image = configuration();
        let capabilities = image.input_capabilities.as_mut().expect("inputs");
        capabilities
            .supported_modalities
            .push(ModelInputModality::Image as i32);
        capabilities.image_input_capacity = Some(ModelImageInputCapacity::default());
        assert_eq!(
            messages(validate_model_configuration(&model(), &image, &release())),
            ["configuration \"omlx-apple\" declares image input, which oMLX does not serve"]
        );
    }

    #[test]
    fn the_context_per_request_is_the_total_context_split_among_the_parallel_requests() {
        for total_context in [65_536, 65_537] {
            validate_parameter_values(&unsigned_parameter_values([
                (TOTAL_CONTEXT_PARAMETER_ID, total_context),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
                (PARALLEL_REQUESTS_PARAMETER_ID, 2),
            ]))
            .expect("the even split, rounded down");
        }
        validate_parameter_values(&unsigned_parameter_values([(
            CONTEXT_PER_REQUEST_PARAMETER_ID,
            32_768,
        )]))
        .expect("values left for resolution to derive");

        for (total_context, split) in [(65_535, 32_767), (131_072, 65_536)] {
            assert_eq!(
                validate_parameter_values(&unsigned_parameter_values([
                    (TOTAL_CONTEXT_PARAMETER_ID, total_context),
                    (CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 2),
                ]))
                .expect_err("a total context splitting into another context per request")
                .to_string(),
                format!(
                    "context per request of 32768 tokens is not the total context of {total_context} tokens split among 2 parallel requests, {split} tokens each; oMLX lets each parallel request use its full context per request, so leave the total context out or set it to 65536 tokens"
                )
            );
        }
    }

    #[test]
    fn resolution_derives_the_left_out_context_value_from_the_others() {
        let schema = release().parameter_schema.expect("schema");
        let derived = |values: &[(&str, u64)]| {
            crate::model::engine::derived_context_values(
                EngineKind::Omlx as i32,
                &model(),
                &configuration(),
                &schema,
                &unsigned_parameter_values(values.iter().copied()),
            )
        };

        assert_eq!(
            derived(&[
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 16_384),
                (PARALLEL_REQUESTS_PARAMETER_ID, 3),
            ]),
            unsigned_parameter_values([(TOTAL_CONTEXT_PARAMETER_ID, 49_152)]).values
        );
        assert_eq!(
            derived(&[
                (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                (PARALLEL_REQUESTS_PARAMETER_ID, 2),
            ]),
            unsigned_parameter_values([(CONTEXT_PER_REQUEST_PARAMETER_ID, 32_768)]).values
        );
    }

    fn messages(errors: Vec<anyhow::Error>) -> Vec<String> {
        errors.into_iter().map(|error| error.to_string()).collect()
    }

    fn parameter<'a>(
        release: &'a mut EngineRelease,
        parameter_id: &str,
    ) -> &'a mut ParameterDefinition {
        release
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .iter_mut()
            .find(|definition| definition.parameter_id == parameter_id)
            .expect("parameter")
    }

    fn payload(configuration: &mut ModelEngineConfiguration) -> &mut OmlxModelConfiguration {
        match configuration
            .engine_configuration
            .as_mut()
            .and_then(|payload| payload.configuration.as_mut())
        {
            Some(Configuration::Omlx(payload)) => payload,
            _ => panic!("oMLX payload"),
        }
    }

    fn fixed(configuration: &mut ModelEngineConfiguration) -> &mut OmlxFixedModelSettings {
        payload(configuration)
            .fixed_settings
            .as_mut()
            .expect("fixed settings")
    }

    fn snapshot_files(
        model: &mut Model,
    ) -> &mut Vec<dotcore_proto::dotcore::catalog::v1::HuggingFaceSnapshotFile> {
        match model.artifacts[0]
            .source
            .as_mut()
            .and_then(|source| source.content.as_mut())
        {
            Some(HuggingFaceContent::Snapshot(snapshot)) => &mut snapshot.files,
            _ => panic!("snapshot artifact"),
        }
    }
}
