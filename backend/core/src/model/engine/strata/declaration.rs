//! Strata-specific validation of catalog engine releases and model engine configurations.
//!
//! A release must declare well-formed unsigned-integer parameters, at most the parallel requests
//! Strata's start wrapper accepts, Linux artifacts only, and the deployment's data folder the
//! wrapper writes to. A configuration must bind model artifacts shaped as its fixed settings say,
//! with the draft layer they name, run text-only on exactly one NVIDIA accelerator, declare no
//! options, and reference every other release parameter. Launch values must give Strata a total
//! context that is exactly what its parallel requests hold.

use std::collections::BTreeSet;

use dotcore_catalog::shared_parameters::{
    self, CONTEXT_PER_REQUEST_PARAMETER_ID, PARALLEL_REQUESTS_PARAMETER_ID,
    TOTAL_CONTEXT_PARAMETER_ID, is_shared_parameter,
};
use dotcore_domain::Accelerator;
use dotcore_proto::dotcore::{
    catalog::v1::{
        EngineParameterSchema, EngineParameterValue, EngineParameterValues, EngineRelease,
        HardwareClass, HuggingFaceArtifactSource, Model, ModelEngineConfiguration,
        ModelInputModality, ModelParallelShape, StorageScope,
        engine_parameter_definition::ParameterType,
        engine_parameter_value::Value as EngineParameterValueKind,
        hugging_face_artifact_source::Content as HuggingFaceContent,
    },
    engine::{
        strata::v1::{
            StrataFixedModelSettings, StrataModelConfiguration, StrataReadyPack,
            strata_fixed_model_settings::ModelFiles,
        },
        v1::{EngineKind, engine_model_configuration::Configuration},
    },
};

use crate::model::engine::{
    declaration::classes_outside_vendors,
    definition::{unsigned_parameter_maximum, unsigned_parameter_value},
    split_gguf::split_gguf_entry,
};

/// Largest parallel request count Strata's start wrapper accepts.
const MAXIMUM_PARALLEL_REQUESTS: u64 = 8;
/// Container folder where Strata's start wrapper keeps the deployment's tokenizer, run
/// configuration and engine log.
const DATA_MOUNT_PATH: &str = "/data";
const MODEL_FILE_EXTENSION: &str = ".gguf";
/// Files Strata's engine reads from a draft layer's folder.
const DRAFT_LAYER_FILES: [&str; 3] = ["dense.bin", "dense.txt", "experts.bin"];
/// Files within a ready-made pack's folder the start wrapper needs.
const READY_PACK_FILES: [&str; 2] = ["native_experts.txt", "tokenizer/vocab.json"];

/// Context per request for values that leave it out: the total context split evenly among the
/// parallel requests, capped by the maximum context, since Strata holds a full context per request
/// for each of them. Returns none when the shared rule would fill nothing.
pub(crate) fn filled_context_per_request(
    schema: &EngineParameterSchema,
    values: &EngineParameterValues,
    maximum_context_tokens: u32,
) -> Option<EngineParameterValue> {
    // The shared rule with no maximum yields the total context, or none when it fills nothing.
    let total_context = shared_parameters::filled_context_per_request(schema, values, u32::MAX)
        .and_then(|filled| match filled.value {
            Some(EngineParameterValueKind::UnsignedInteger(total_context)) => Some(total_context),
            _ => None,
        })?;
    let parallel_requests = unsigned_parameter_value(values, PARALLEL_REQUESTS_PARAMETER_ID)
        .unwrap_or(1)
        .max(1);
    Some(EngineParameterValue {
        parameter_id: CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
        value: Some(EngineParameterValueKind::UnsignedInteger(
            (total_context / parallel_requests).min(u64::from(maximum_context_tokens)),
        )),
    })
}

/// Rejects a context per request other than the total context split evenly among the parallel
/// requests, rounded down: Strata holds that much for each parallel request, at most the total
/// context sizing counts. A split above the configuration's maximum context can't launch at all.
pub(crate) fn validate_parameter_values(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    values: &EngineParameterValues,
) -> anyhow::Result<()> {
    let (Some(total_context), Some(context_per_request), Some(parallel_requests)) = (
        unsigned_parameter_value(values, TOTAL_CONTEXT_PARAMETER_ID),
        unsigned_parameter_value(values, CONTEXT_PER_REQUEST_PARAMETER_ID),
        unsigned_parameter_value(values, PARALLEL_REQUESTS_PARAMETER_ID),
    ) else {
        return Ok(());
    };
    let split = total_context / parallel_requests.max(1);
    let maximum = u64::from(shared_parameters::maximum_context_tokens(
        model,
        configuration,
    ));
    if split > maximum {
        anyhow::bail!(
            "total context of {total_context} tokens gives each of {parallel_requests} parallel requests {split} tokens, above the maximum context of {maximum} tokens; Strata holds a full context per request for each parallel request, so lower the total context to at most {} tokens or raise the parallel requests",
            maximum.saturating_mul(parallel_requests)
        );
    }
    if context_per_request != split {
        anyhow::bail!(
            "context per request of {context_per_request} tokens is not the total context of {total_context} tokens split among {parallel_requests} parallel requests, {split} tokens each; Strata holds a full context per request for each parallel request, so leave the context per request out or set it to {split} tokens"
        );
    }
    Ok(())
}

/// Accepts a release of non-empty unsigned-integer parameters that declares at most the parallel
/// requests the start wrapper accepts, ships only Linux artifacts, and declares a
/// deployment-scoped storage at the wrapper's data folder. The catalog checks the shared
/// parameters' fixed shape.
pub(crate) fn validate_engine_release(engine: &EngineRelease) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if engine.engine_kind != EngineKind::Strata as i32 {
        errors.push(anyhow::anyhow!(
            "Strata engine release has a mismatched engine kind"
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
            "Strata engine release requires non-empty unsigned-integer parameter definitions"
        ));
    }
    if unsigned_parameter_maximum(engine, PARALLEL_REQUESTS_PARAMETER_ID)
        .is_none_or(|maximum| maximum > MAXIMUM_PARALLEL_REQUESTS)
    {
        errors.push(anyhow::anyhow!(
            "Strata engine release must declare {PARALLEL_REQUESTS_PARAMETER_ID} up to \
             {MAXIMUM_PARALLEL_REQUESTS}"
        ));
    }
    let linux = engine.artifacts.iter().all(|artifact| {
        !artifact.selectors.is_empty()
            && artifact.selectors.iter().all(|selector| {
                selector
                    .platform
                    .as_ref()
                    .is_some_and(|platform| platform.operating_system == "linux")
            })
    });
    if !linux {
        errors.push(anyhow::anyhow!(
            "Strata engine release supports only Linux artifacts"
        ));
    }
    let data_folder = engine.storage_bindings.iter().any(|binding| {
        binding.mount_path == DATA_MOUNT_PATH && binding.scope == StorageScope::Deployment as i32
    });
    if !data_folder {
        errors.push(anyhow::anyhow!(
            "Strata engine release must declare deployment-scoped storage at {DATA_MOUNT_PATH}"
        ));
    }
    errors
}

/// Accepts supported hardware classes that select only NVIDIA accelerators. Selectors match
/// single-device targets only, so Strata runs on exactly one NVIDIA accelerator.
pub(crate) fn validate_hardware_support(
    configuration: &ModelEngineConfiguration,
    hardware_classes: &[&HardwareClass],
) -> Vec<anyhow::Error> {
    classes_outside_vendors(hardware_classes, &[Accelerator::Nvidia])
        .into_iter()
        .map(|hardware_class_id| {
            anyhow::anyhow!(
                "configuration {:?} hardware class {hardware_class_id:?} must select only NVIDIA accelerators for Strata",
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
    if engine.engine_kind != EngineKind::Strata as i32
        || configuration
            .engine_configuration
            .as_ref()
            .is_none_or(|wrapper| wrapper.engine_kind != EngineKind::Strata as i32)
    {
        errors.push(anyhow::anyhow!(
            "Strata configuration has a mismatched engine wrapper or release kind"
        ));
    }
    let Some(payload) = configuration.engine_configuration.as_ref() else {
        return errors;
    };
    let Some(Configuration::Strata(payload)) = payload.configuration.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has a payload that does not match Strata",
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
    payload: &StrataModelConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(fixed) = payload.fixed_settings.as_ref() else {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires fixed Strata settings",
            configuration.configuration_id
        ));
        return;
    };
    if !super::valid_fixed_settings(fixed) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid fixed Strata settings: the architecture takes \
             lowercase letters, digits and '_', the MTP draft depth is 1 to {}, the model files \
             are declared, a pack path names its draft layer, and a ready-made pack names plain \
             relative paths",
            configuration.configuration_id,
            super::MAXIMUM_MTP_DRAFT_TOKENS
        ));
    }

    validate_parameters(configuration, engine, fixed, errors);
    validate_artifacts(model, configuration, fixed, errors);
    validate_execution_shape(configuration, errors);
    validate_inputs_and_options(configuration, errors);
}

fn validate_parameters(
    configuration: &ModelEngineConfiguration,
    engine: &EngineRelease,
    fixed: &StrataFixedModelSettings,
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
    let expert_vram = fixed.expert_vram_mib_parameter_id.as_str();
    if !declared.contains(expert_vram) || is_shared_parameter(expert_vram) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has an invalid Strata engine parameter reference",
            configuration.configuration_id
        ));
    }

    // Every release parameter must reach the launch plan, so none can be set without effect. The
    // total context reaches it through the parallel requests and the context per request.
    let launched = BTreeSet::from([
        expert_vram,
        TOTAL_CONTEXT_PARAMETER_ID,
        CONTEXT_PER_REQUEST_PARAMETER_ID,
        PARALLEL_REQUESTS_PARAMETER_ID,
    ]);
    if declared != launched {
        errors.push(anyhow::anyhow!(
            "configuration {:?} must reference every Strata engine release parameter",
            configuration.configuration_id
        ));
    }
}

/// Accepts bindings whose model artifacts have the shape the fixed settings' model files declare,
/// each selecting exactly the draft layer the settings name, if any.
fn validate_artifacts(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    fixed: &StrataFixedModelSettings,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(model_files) = fixed.model_files.as_ref() else {
        return;
    };
    let draft_layer = super::draft_layer_artifact_id(fixed);
    if configuration.artifact_bindings.is_empty() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} binds no model artifact",
            configuration.configuration_id
        ));
    }
    for binding in &configuration.artifact_bindings {
        if binding
            .auxiliary_artifact_ids
            .iter()
            .map(String::as_str)
            .ne(draft_layer)
        {
            errors.push(anyhow::anyhow!(
                "configuration {:?} binding {:?} must select exactly the draft layer its fixed \
                 Strata settings name, if any",
                configuration.configuration_id,
                binding.binding_id
            ));
        }
        let content = model
            .artifacts
            .iter()
            .find(|artifact| artifact.artifact_id == binding.model_artifact_id)
            .and_then(|artifact| artifact.source.as_ref())
            .and_then(|source| source.content.as_ref());
        let valid = match (model_files, content) {
            (ModelFiles::SingleGguf(_), Some(HuggingFaceContent::File(file))) => {
                file.path.ends_with(MODEL_FILE_EXTENSION)
            }
            (ModelFiles::SplitGguf(_), Some(HuggingFaceContent::Snapshot(snapshot))) => {
                split_gguf_entry(snapshot.files.iter().map(|file| file.path.as_str())).is_ok()
            }
            (ModelFiles::ReadyPack(pack), Some(HuggingFaceContent::Snapshot(snapshot))) => {
                ready_pack_files(pack)
                    .all(|path| snapshot.files.iter().any(|file| file.path == path))
            }
            _ => false,
        };
        let shape = match model_files {
            ModelFiles::SingleGguf(_) => "a single GGUF file",
            ModelFiles::SplitGguf(_) => "one complete set of split GGUF shards",
            ModelFiles::ReadyPack(_) => {
                "a snapshot holding the ready-made pack and the GGUF files its fixed settings name"
            }
        };
        if !valid {
            errors.push(anyhow::anyhow!(
                "configuration {:?} binding {:?} requires a model artifact that is {shape}",
                configuration.configuration_id,
                binding.binding_id
            ));
        }
    }
    if let Some(draft_layer) = draft_layer
        && !model
            .auxiliary_artifacts
            .iter()
            .find(|artifact| artifact.auxiliary_artifact_id == draft_layer)
            .and_then(|artifact| artifact.source.as_ref())
            .is_some_and(valid_draft_layer)
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} draft layer {draft_layer:?} must be a model auxiliary artifact \
             holding {} in one folder",
            configuration.configuration_id,
            DRAFT_LAYER_FILES.join(", ")
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
            "configuration {:?} supports only one-target Strata execution",
            configuration.configuration_id
        ));
    }
}

/// Strata's start wrapper fixes speculative decoding and serves text only, so a configuration
/// has no options to realize and no image input.
fn validate_inputs_and_options(
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    if !configuration.options.is_empty() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} declares options, which Strata does not realize",
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
            "configuration {:?} declares image input, which Strata does not serve",
            configuration.configuration_id
        ));
    }
}

/// The files a ready-made pack's snapshot must hold: its GGUF files and its pack's own files.
fn ready_pack_files(pack: &StrataReadyPack) -> impl Iterator<Item = String> + '_ {
    [
        &pack.dense_gguf_path,
        &pack.lookup_table_gguf_path,
        &pack.token_embedding_gguf_path,
    ]
    .into_iter()
    .cloned()
    .chain(
        READY_PACK_FILES
            .iter()
            .map(|file| format!("{}/{file}", pack.pack_path)),
    )
}

/// Whether a draft layer is a snapshot whose files sit in one folder and include every file the
/// engine reads from it.
fn valid_draft_layer(source: &HuggingFaceArtifactSource) -> bool {
    let Some(HuggingFaceContent::Snapshot(snapshot)) = source.content.as_ref() else {
        return false;
    };
    let (folders, names): (BTreeSet<_>, BTreeSet<_>) = snapshot
        .files
        .iter()
        .map(|file| file.path.rsplit_once('/').unwrap_or(("", &file.path)))
        .unzip();
    folders.len() == 1 && DRAFT_LAYER_FILES.iter().all(|name| names.contains(name))
}

#[cfg(test)]
mod tests {
    use dotcore_catalog::{CatalogModelId, test_support::vendor_hardware_class};
    use dotcore_domain::ExecutionPlatform;
    use dotcore_proto::dotcore::{
        catalog::v1::{
            BooleanEngineParameterSchema, EngineParameterDefinition, HuggingFaceArtifactSource,
            HuggingFaceFileArtifact, ModelConfigurationOption, ModelImageInputCapacity,
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
            strata::{
                DRAFT_LAYER_ARTIFACT_ID, EXPERT_VRAM_MIB_PARAMETER_ID, configuration, model,
                ready_pack_configuration, release, split_gguf_configuration,
            },
            unsigned_parameter_values,
        },
    };

    #[test]
    fn accepts_the_synthetic_release_and_configuration() {
        assert!(
            validate_engine_release(&release()).is_empty(),
            "{:?}",
            validate_engine_release(&release())
        );
        for configuration in [
            configuration(),
            split_gguf_configuration(),
            ready_pack_configuration(),
        ] {
            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration,
                    &release()
                )),
                Vec::<String>::new(),
                "{}",
                configuration.configuration_id
            );
        }
    }

    #[test]
    fn rejects_release_with_wrong_kind_or_unsupported_parameter_schema() {
        let mut wrong_kind = release();
        wrong_kind.engine_kind = EngineKind::Ninfer as i32;
        assert_eq!(
            messages(validate_engine_release(&wrong_kind)),
            ["Strata engine release has a mismatched engine kind"]
        );

        let mut boolean = release();
        parameter(&mut boolean, EXPERT_VRAM_MIB_PARAMETER_ID).parameter_type =
            Some(ParameterType::Boolean(BooleanEngineParameterSchema {}));
        assert_eq!(
            messages(validate_engine_release(&boolean)),
            ["Strata engine release requires non-empty unsigned-integer parameter definitions"]
        );
    }

    #[test]
    fn rejects_parallel_requests_missing_or_above_the_wrapper_limit() {
        let mut above = release();
        parameter(&mut above, PARALLEL_REQUESTS_PARAMETER_ID).parameter_type =
            Some(unsigned(1, 9, "requests"));
        let mut missing = release();
        missing
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .retain(|definition| definition.parameter_id != PARALLEL_REQUESTS_PARAMETER_ID);

        for release in [above, missing] {
            assert_eq!(
                messages(validate_engine_release(&release)),
                ["Strata engine release must declare parallel-requests up to 8"]
            );
        }
    }

    #[test]
    fn rejects_release_artifacts_other_than_linux() {
        let mut release = release();
        release
            .artifacts
            .push(dotcore_domain::test_support::oci::engine_artifact(
                "darwin",
                Some("registry.dotcore.test"),
                "strata",
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "darwin",
                "arm64",
                "apple",
            ));

        assert_eq!(
            messages(validate_engine_release(&release)),
            ["Strata engine release supports only Linux artifacts"]
        );
    }

    #[test]
    fn rejects_release_without_deployment_storage_at_the_data_folder() {
        let mut elsewhere = release();
        elsewhere.storage_bindings[0].mount_path = "/cache".to_owned();
        let mut shared = release();
        shared.storage_bindings[0].scope = StorageScope::EngineArtifact as i32;
        let mut none = release();
        none.storage_bindings.clear();

        for release in [elsewhere, shared, none] {
            assert_eq!(
                messages(validate_engine_release(&release)),
                ["Strata engine release must declare deployment-scoped storage at /data"]
            );
        }
    }

    #[test]
    fn rejects_expert_vram_references_that_are_missing_or_shared() {
        for reference in ["missing", TOTAL_CONTEXT_PARAMETER_ID] {
            let mut configuration = configuration();
            fixed_settings(&mut configuration).expert_vram_mib_parameter_id = reference.to_owned();

            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration,
                    &release()
                )),
                [
                    "configuration \"strata-nvidia\" has an invalid Strata engine parameter reference",
                    "configuration \"strata-nvidia\" must reference every Strata engine release parameter",
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
                parameter_id: "kv-bits".to_owned(),
                parameter_type: Some(unsigned(4, 16, "bits")),
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
                "configuration \"strata-nvidia\" must reference every Strata engine release parameter"
            ]
        );
    }

    #[test]
    fn rejects_model_artifacts_outside_the_declared_model_files() {
        let mut ninfer = model();
        ninfer.artifacts[0].source = Some(file_source("weights.ninfer"));
        assert_eq!(
            messages(validate_model_configuration(
                &ninfer,
                &configuration(),
                &release()
            )),
            [
                "configuration \"strata-nvidia\" binding \"fixture-strata-artifact\" requires a model artifact that is a single GGUF file"
            ]
        );

        let mut single_bound_to_split = configuration();
        single_bound_to_split.artifact_bindings[0].model_artifact_id =
            "fixture-split-gguf".to_owned();
        assert_eq!(
            messages(validate_model_configuration(
                &model(),
                &single_bound_to_split,
                &release()
            )),
            [
                "configuration \"strata-nvidia\" binding \"fixture-strata-artifact\" requires a model artifact that is a single GGUF file"
            ]
        );

        let mut missing_shard = model();
        snapshot_files(&mut missing_shard.artifacts[1].source).pop();
        assert_eq!(
            messages(validate_model_configuration(
                &missing_shard,
                &split_gguf_configuration(),
                &release()
            )),
            [
                "configuration \"strata-fixture-split-gguf\" binding \"fixture-split-gguf\" requires a model artifact that is one complete set of split GGUF shards"
            ]
        );

        let mut no_tokenizer = model();
        snapshot_files(&mut no_tokenizer.artifacts[2].source)
            .retain(|file| file.path != "pack/tokenizer/vocab.json");
        assert_eq!(
            messages(validate_model_configuration(
                &no_tokenizer,
                &ready_pack_configuration(),
                &release()
            )),
            [
                "configuration \"strata-fixture-ready-pack\" binding \"fixture-ready-pack\" requires a model artifact that is a snapshot holding the ready-made pack and the GGUF files its fixed settings name"
            ]
        );
    }

    #[test]
    fn bindings_select_exactly_the_declared_draft_layer() {
        let mut single_with_draft = configuration();
        single_with_draft.artifact_bindings[0]
            .auxiliary_artifact_ids
            .push(DRAFT_LAYER_ARTIFACT_ID.to_owned());
        let mut split_without_draft = split_gguf_configuration();
        split_without_draft.artifact_bindings[0]
            .auxiliary_artifact_ids
            .clear();

        for configuration in [single_with_draft, split_without_draft] {
            let binding = &configuration.artifact_bindings[0].binding_id;
            assert_eq!(
                messages(validate_model_configuration(
                    &model(),
                    &configuration,
                    &release()
                )),
                [format!(
                    "configuration {:?} binding {binding:?} must select exactly the draft layer its fixed Strata settings name, if any",
                    configuration.configuration_id
                )]
            );
        }
    }

    #[test]
    fn the_draft_layer_holds_the_engine_files_in_one_folder() {
        let mut missing = model();
        snapshot_files(&mut missing.auxiliary_artifacts[0].source)
            .retain(|file| file.path != "mtp/dense.txt");
        let mut two_folders = model();
        snapshot_files(&mut two_folders.auxiliary_artifacts[0].source)[0].path =
            "other/dense.bin".to_owned();
        let mut file = model();
        file.auxiliary_artifacts[0].source = Some(file_source("mtp/experts.bin"));

        for model in [missing, two_folders, file] {
            assert_eq!(
                messages(validate_model_configuration(
                    &model,
                    &split_gguf_configuration(),
                    &release()
                )),
                [
                    "configuration \"strata-fixture-split-gguf\" draft layer \"fixture-draft-layer\" must be a model auxiliary artifact holding dense.bin, dense.txt, experts.bin in one folder"
                ]
            );
        }
    }

    #[test]
    fn rejects_other_than_one_target_options_and_image_input() {
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
            ["configuration \"strata-nvidia\" supports only one-target Strata execution"]
        );

        let mut options = configuration();
        options.options.push(ModelConfigurationOption {
            option_id: "mtp".to_owned(),
            ..Default::default()
        });
        assert_eq!(
            messages(validate_model_configuration(&model(), &options, &release())),
            ["configuration \"strata-nvidia\" declares options, which Strata does not realize"]
        );

        let mut image = configuration();
        let inputs = image.input_capabilities.as_mut().expect("inputs");
        inputs
            .supported_modalities
            .push(ModelInputModality::Image as i32);
        inputs.image_input_capacity = Some(ModelImageInputCapacity::default());
        assert_eq!(
            messages(validate_model_configuration(&model(), &image, &release())),
            ["configuration \"strata-nvidia\" declares image input, which Strata does not serve"]
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
                "configuration \"strata-nvidia\" hardware class \"apple\" must select only NVIDIA accelerators for Strata"
            ]
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
            ["configuration \"strata-nvidia\" has a payload that does not match Strata"]
        );
    }

    #[test]
    fn catalog_loads_a_profile_shaped_strata_declaration() {
        let catalog = profile_shaped_catalog();

        let model = catalog
            .list_model_releases()
            .into_iter()
            .find(|model| model.model_id == "fixture-strata-model")
            .expect("Strata model");
        assert_eq!(
            model.configurations[0].engine_configuration,
            configuration().engine_configuration
        );
        let releases = catalog.list_engine_releases();
        assert_eq!(releases[0].parameter_schema, release().parameter_schema);
        assert_eq!(releases[0].storage_bindings, release().storage_bindings);
    }

    #[test]
    fn catalog_rejects_a_profile_that_leaves_the_expert_cap_unset() {
        let model = CATALOG_MODEL.replace(
            "      derivedParameters:\n        expert-vram-mib: \"${expertVramMib}\"\n",
            "",
        );
        assert_ne!(model, CATALOG_MODEL);

        let error = load_catalog(&model).expect_err("no expert cap");

        assert!(
            format!("{error:#}").contains(
                "profile \"strata-q4\" omits engine parameter \"expert-vram-mib\", which binding \"fixture-strata-artifact\" does not derive on hardware class \"nvidia\""
            ),
            "{error:#}"
        );
    }

    #[test]
    fn engine_defaults_launch_one_request_with_the_expert_cap_from_sizing() {
        let catalog = profile_shaped_catalog();

        let launch = launch(&catalog, &[]).expect("Strata launch at engine defaults");

        assert_eq!(
            launch_argv(&launch),
            [
                "/usr/bin/tini",
                "--",
                "/usr/local/bin/dotcore-strata-start",
                "--model",
                "--model-name",
                "fixture-strata-model",
                "--arch",
                "qwen35moe",
                "--mtp",
                "2",
                "--context",
                "32768",
                "--parallel",
                "1",
                "--expert-vram-mib",
                "8192",
                "--port",
                "8080",
            ]
        );
        // 1000 + 10 × 32768 + 8192 MiB of experts on the accelerator; the host pins all experts.
        assert_eq!(
            launch.sizing,
            Some(ResolvedSizing {
                accelerator_memory_bytes: Some(1_000 + 10 * 32_768 + 8_192 * 1_048_576),
                derived_engine_parameter_ids: vec![
                    CONTEXT_PER_REQUEST_PARAMETER_ID.to_owned(),
                    EXPERT_VRAM_MIB_PARAMETER_ID.to_owned(),
                ],
                host_memory_bytes: Some(20_000 * 1_048_576),
            })
        );
    }

    #[test]
    fn total_context_of_every_parallel_request_launches_with_a_supplied_expert_cap() {
        let catalog = profile_shaped_catalog();

        let launch = launch(
            &catalog,
            &[
                (TOTAL_CONTEXT_PARAMETER_ID, 262_144),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 131_072),
                (PARALLEL_REQUESTS_PARAMETER_ID, 2),
                (EXPERT_VRAM_MIB_PARAMETER_ID, 4_096),
            ],
        )
        .expect("two requests at their full context");

        assert_eq!(
            launch_flags(&launch),
            [
                ("--context", "131072".to_owned()),
                ("--parallel", "2".to_owned()),
                ("--expert-vram-mib", "4096".to_owned()),
            ]
        );
        assert_eq!(
            launch
                .sizing
                .as_ref()
                .map(|sizing| sizing.derived_engine_parameter_ids.clone()),
            Some(Vec::new())
        );
    }

    #[test]
    fn rejects_a_context_per_request_other_than_the_total_split_among_parallel_requests() {
        let catalog = profile_shaped_catalog();

        for (context_per_request, split) in [(16_384, 32_768), (65_536, 32_768)] {
            assert_eq!(
                launch(
                    &catalog,
                    &[
                        (TOTAL_CONTEXT_PARAMETER_ID, 65_536),
                        (CONTEXT_PER_REQUEST_PARAMETER_ID, context_per_request),
                        (PARALLEL_REQUESTS_PARAMETER_ID, 2),
                    ],
                )
                .expect_err("context per request unlike the split")
                .to_string(),
                format!(
                    "invalid engine parameter values: context per request of {context_per_request} tokens is not the total context of 65536 tokens split among 2 parallel requests, {split} tokens each; Strata holds a full context per request for each parallel request, so leave the context per request out or set it to {split} tokens"
                )
            );
        }
    }

    #[test]
    fn a_left_out_context_per_request_splits_the_total_context_among_parallel_requests() {
        let catalog = profile_shaped_catalog();

        let split = launch(&catalog, &[(PARALLEL_REQUESTS_PARAMETER_ID, 2)])
            .expect("engine default total context for two requests");
        assert_eq!(
            launch_flags(&split),
            [
                ("--context", "16384".to_owned()),
                ("--parallel", "2".to_owned()),
                ("--expert-vram-mib", "8192".to_owned()),
            ]
        );

        // 32,768 doesn't divide by 3: each request holds 10,922 tokens and sizing counts all 32,768.
        let uneven =
            launch(&catalog, &[(PARALLEL_REQUESTS_PARAMETER_ID, 3)]).expect("a split rounded down");
        assert_eq!(
            launch_flags(&uneven),
            [
                ("--context", "10922".to_owned()),
                ("--parallel", "3".to_owned()),
                ("--expert-vram-mib", "8192".to_owned()),
            ]
        );
        assert_eq!(
            uneven
                .sizing
                .as_ref()
                .and_then(|sizing| sizing.accelerator_memory_bytes),
            Some(1_000 + 10 * 32_768 + 8_192 * 1_048_576)
        );
        launch(
            &catalog,
            &[
                (TOTAL_CONTEXT_PARAMETER_ID, 32_768),
                (CONTEXT_PER_REQUEST_PARAMETER_ID, 10_922),
                (PARALLEL_REQUESTS_PARAMETER_ID, 3),
            ],
        )
        .expect("the rounded-down split supplied explicitly");
    }

    #[test]
    fn rejects_a_total_context_whose_split_exceeds_the_maximum_context() {
        let catalog = profile_shaped_catalog();

        assert_eq!(
            launch(
                &catalog,
                &[
                    (TOTAL_CONTEXT_PARAMETER_ID, 1_048_576),
                    (PARALLEL_REQUESTS_PARAMETER_ID, 2),
                ],
            )
            .expect_err("half the total context above the maximum context")
            .to_string(),
            "invalid engine parameter values: total context of 1048576 tokens gives each of 2 parallel requests 524288 tokens, above the maximum context of 262144 tokens; Strata holds a full context per request for each parallel request, so lower the total context to at most 524288 tokens or raise the parallel requests"
        );
    }

    const CATALOG_ENGINE: &str = r#"
engineReleaseId: "fixture-strata"
engineKind: "ENGINE_KIND_STRATA"
version: "1"
parameterSchema:
  schemaId: "fixture-strata-v1"
  parameters:
    - { parameterId: "total-context", displayName: "Total context", exposed: true, description: "total-context", unsignedInteger: { minimum: "1024", maximum: "2097152", unit: "tokens", defaultValue: "32768" } }
    - { parameterId: "context-per-request", displayName: "Context per request", exposed: true, description: "context-per-request", unsignedInteger: { minimum: "1024", maximum: "262144", unit: "tokens" } }
    - { parameterId: "parallel-requests", displayName: "Parallel requests", exposed: true, description: "parallel-requests", unsignedInteger: { minimum: "1", maximum: "8", unit: "requests", defaultValue: "1" } }
    - { parameterId: "expert-vram-mib", displayName: "Expert accelerator memory", exposed: true, description: "expert-vram-mib", unsignedInteger: { minimum: "1", maximum: "1048576", unit: "MiB" } }
storageBindings:
  - { bindingName: "data", mountPath: "/data", kind: "STORAGE_KIND_CACHE", scope: "STORAGE_SCOPE_DEPLOYMENT" }
artifacts:
  - artifactId: "linux-amd64-cuda"
    oci:
      repository: { registry: { host: "registry.dotcore.test" }, name: "strata" }
      digest: { algorithm: "sha256", encoded: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" }
    selectors:
      - platform: { operatingSystem: "linux", architecture: "amd64" }
        accelerators: ["nvidia"]
        minimumNvidiaDriverMajorVersion: 580
"#;

    const CATALOG_MODEL: &str = r#"
modelId: "fixture-strata-model"
displayName: "Fixture Strata model"
version: "1"
maximumContextTokens: 262144
artifacts:
  - artifactId: "fixture-strata-artifact"
    weightQuantization: "Q4_K_XL"
    source:
      repository: "dotcore/fixture-strata"
      revision: "0123456789abcdef0123456789abcdef01234567"
      file:
        path: "fixture-Q4_K_XL.gguf"
        provenanceDigest: "74d2c57145e6ff11d1d2faa79594477f9bc903a611af1fb20218189fbbb77d82"
configurations:
  - configurationId: "strata-nvidia"
    engineArtifacts: [{ engineReleaseId: "fixture-strata", engineArtifactId: "linux-amd64-cuda" }]
    artifactBindings: [{ bindingId: "fixture-strata-artifact", modelArtifactId: "fixture-strata-artifact" }]
    sizing:
      parameters: { baseBytes: 1000, bytesPerKvToken: 10, expertMib: 20000, expertVramMib: 8192 }
      derivedParameters:
        expert-vram-mib: "${expertVramMib}"
      estimates:
        acceleratorMemory:
          unit: bytes
          equation: "${baseBytes} + ${bytesPerKvToken} * ${total-context} + ${expert-vram-mib} * MiB"
        hostMemory:
          unit: bytes
          equation: "${expertMib} * MiB"
    hardwareSupport:
      - hardwareClassId: "nvidia"
        hardwareRequirements:
          minimumMemoryBytesPerDevice: "32000000000"
          minimumSystemMemoryBytes: "64000000000"
        bindingSizing:
          - bindingId: "fixture-strata-artifact"
    engineConfiguration:
      engineKind: "ENGINE_KIND_STRATA"
      strata:
        fixedSettings:
          expertVramMibParameterId: "expert-vram-mib"
          architecture: "qwen35moe"
          mtpDraftTokens: 2
          singleGguf: {}
    executionCapabilities: { supportedParallelShapes: ["MODEL_PARALLEL_SHAPE_ONE"] }
    requestKinds: ['INFERENCE_REQUEST_KIND_CHAT']
    inputCapabilities:
      supportedModalities: ["MODEL_INPUT_MODALITY_TEXT"]
profiles:
  - profileId: "strata-q4"
    displayName: "Q4 Strata"
    description: "Runs the fixture through Strata."
    configurationId: "strata-nvidia"
    defaultModelArtifactBindingId: "fixture-strata-artifact"
    engineParameterDefaults: {}
    "#;

    /// Loads the profile-shaped engine and model with the shared hardware classes.
    fn profile_shaped_catalog() -> dotcore_catalog::Catalog {
        load_catalog(CATALOG_MODEL).expect("Strata catalog")
    }

    /// Loads the engine and the given model with the shared hardware classes.
    fn load_catalog(model: &str) -> anyhow::Result<dotcore_catalog::Catalog> {
        let root = tempfile::tempdir().expect("Catalog root");
        for (path, content) in [
            ("engines/strata.yaml", CATALOG_ENGINE),
            ("models/strata-model.yaml", model),
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
        .map_err(anyhow::Error::new)
    }

    /// Resolves the profile on an NVIDIA target with the supplied engine parameter values after
    /// the profile and engine defaults fill the rest, as the controller does.
    fn launch(
        catalog: &dotcore_catalog::Catalog,
        values: &[(&str, u64)],
    ) -> Result<ResolvedLocalModelDeploymentSnapshot, ModelLaunchResolutionError> {
        let model_id = CatalogModelId("fixture-strata-model".to_owned());
        let configuration = apply_profile_defaults(
            catalog,
            &model_id,
            "strata-q4",
            &LocalModelDeploymentConfiguration {
                model_artifact_binding_id: "fixture-strata-artifact".to_owned(),
                engine_parameter_values: Some(unsigned_parameter_values(values.iter().copied())),
                ..Default::default()
            },
        )?;
        let (target, inventory) =
            crate::test_support::execution_target_capabilities(ExecutionPlatform::linux_amd64());

        resolve_local_launch(
            catalog,
            &model_id,
            "strata-q4",
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

    /// Context per request, parallel request and expert cap options of the launch plan, each with
    /// its value.
    fn launch_flags(launch: &ResolvedLocalModelDeploymentSnapshot) -> Vec<(&'static str, String)> {
        let argv = launch_argv(launch);
        ["--context", "--parallel", "--expert-vram-mib"]
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

    fn parameter<'a>(
        release: &'a mut EngineRelease,
        parameter_id: &str,
    ) -> &'a mut EngineParameterDefinition {
        release
            .parameter_schema
            .as_mut()
            .expect("schema")
            .parameters
            .iter_mut()
            .find(|definition| definition.parameter_id == parameter_id)
            .expect("parameter")
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

    fn snapshot_files(
        source: &mut Option<HuggingFaceArtifactSource>,
    ) -> &mut Vec<dotcore_proto::dotcore::catalog::v1::HuggingFaceSnapshotFile> {
        match source.as_mut().and_then(|source| source.content.as_mut()) {
            Some(HuggingFaceContent::Snapshot(snapshot)) => &mut snapshot.files,
            _ => panic!("snapshot source"),
        }
    }

    fn fixed_settings(
        configuration: &mut ModelEngineConfiguration,
    ) -> &mut StrataFixedModelSettings {
        match configuration
            .engine_configuration
            .as_mut()
            .and_then(|wrapper| wrapper.configuration.as_mut())
        {
            Some(Configuration::Strata(payload)) => {
                payload.fixed_settings.as_mut().expect("fixed settings")
            }
            _ => panic!("Strata payload"),
        }
    }
}
