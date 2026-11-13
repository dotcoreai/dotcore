//! Validates one model declaration: its identity, artifacts, engine configurations,
//! and profiles.
//!
//! It reports every problem it finds, for example a configuration that references a
//! missing engine release or a binding whose model artifact is undeclared. The same engine
//! parameter values are also re-checked against the engine schema when a deployment
//! snapshot is captured, so the rules apply identically at load time and at snapshot
//! capture.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_domain::valid_id;
use dotcore_proto::dotcore::catalog::v1::{
    EngineArtifact, EngineRelease, HardwareClass, HuggingFaceArtifactSource, InferenceRequestKind,
    IntelligenceScore, License, Model, ModelArtifact, ModelConfigurationOption,
    ModelEngineConfiguration, ModelInputCapabilities, ModelInputModality,
    ModelOptionParameterDefinition, ModelProfile, ParameterValue, ParameterValues,
    SpeculativeDecodingMethod, hugging_face_artifact_source::Content as HuggingFaceContent,
    model_option::Kind as ModelOptionKind,
    model_option_parameter_definition::ParameterType as ModelOptionParameterType,
    parameter_definition::ParameterType as EngineParameterType,
    parameter_value::Value as EngineParameterValueKind,
};

use super::common::lower_hex;
use crate::{
    options::{
        KV_CACHE_PRECISION_PARAMETER_ID, SPECULATIVE_TOKEN_COUNT_PARAMETER_ID,
        reject_multiple_active_speculative_options, resolve_model_option_values,
        valid_option_values,
    },
    selector::{SelectorRelation, selector_relation},
    shared_parameters::{profile_may_default, profile_visibility_problem},
    typed_parameters::{check_value, default_value, is_secret},
    validation::{EngineDeclarationValidator, configuration_engine_release},
};

const MAX_WEIGHT_QUANTIZATION_BYTES: usize = 128;
const CUSTOM_LICENSE_ID: &str = "custom";
const LICENSE_REF_PREFIX: &str = "LicenseRef-";
const SPECULATIVE_TOKEN_COUNT_UNIT: &str = "tokens";

pub fn validate_model(
    model: &Model,
    engines: &BTreeMap<String, EngineRelease>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
    engine_validator: &dyn EngineDeclarationValidator,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    if !valid_id(&model.model_id) || model.display_name.is_empty() || model.version.is_empty() {
        errors.push(anyhow::anyhow!(
            "model requires valid identity, display name, and version"
        ));
    }
    if model.maximum_context_tokens == 0 {
        errors.push(anyhow::anyhow!(
            "model {:?} requires a positive maximum context",
            model.model_id
        ));
    }
    let artifacts = validate_model_artifacts(&model.artifacts, &mut errors);
    let auxiliary = validate_auxiliary_artifacts(model, &mut errors);
    validate_model_options(model, &mut errors);
    if let Some(score) = &model.intelligence {
        validate_intelligence_score(score, &mut errors);
    }
    if let Some(license) = &model.license {
        validate_license(&model.model_id, license, &mut errors);
    }
    let configurations = validate_configurations(
        model,
        &artifacts,
        &auxiliary,
        engines,
        hardware_classes,
        engine_validator,
        &mut errors,
    );
    validate_profiles(model, &configurations, engines, &mut errors);
    errors
}

fn validate_model_artifacts(
    artifacts: &[ModelArtifact],
    errors: &mut Vec<anyhow::Error>,
) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if artifacts.is_empty() {
        errors.push(anyhow::anyhow!("model must contain at least one artifact"));
    }
    for artifact in artifacts {
        if !ids.insert(artifact.artifact_id.clone())
            || !valid_id(&artifact.artifact_id)
            || artifact
                .source
                .as_ref()
                .is_none_or(|value| !valid_pinned_hugging_face_artifact(value))
            || !valid_weight_quantization(&artifact.weight_quantization)
        {
            errors.push(anyhow::anyhow!(
                "model artifact {:?} is invalid",
                artifact.artifact_id
            ));
        }
    }
    ids
}

fn validate_auxiliary_artifacts(
    model: &Model,
    errors: &mut Vec<anyhow::Error>,
) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for artifact in &model.auxiliary_artifacts {
        if !ids.insert(artifact.auxiliary_artifact_id.clone())
            || !valid_id(&artifact.auxiliary_artifact_id)
            || artifact
                .source
                .as_ref()
                .is_none_or(|value| !valid_pinned_hugging_face_artifact(value))
        {
            errors.push(anyhow::anyhow!(
                "model auxiliary artifact {:?} is invalid",
                artifact.auxiliary_artifact_id
            ));
        }
    }
    ids
}

fn validate_model_options(model: &Model, errors: &mut Vec<anyhow::Error>) {
    let mut ids = BTreeSet::new();
    for option in &model.options {
        if !ids.insert(option.option_id.clone())
            || !valid_id(&option.option_id)
            || option.display_name.is_empty()
            || option.kind.is_none()
        {
            errors.push(anyhow::anyhow!(
                "model option {:?} is invalid",
                option.option_id
            ));
        }
        if let Some(ModelOptionKind::SpeculativeDecoding(kind)) = option.kind.as_ref()
            && matches!(
                SpeculativeDecodingMethod::try_from(kind.method),
                Ok(SpeculativeDecodingMethod::Unspecified) | Err(_)
            )
        {
            errors.push(anyhow::anyhow!(
                "model {:?} speculative decoding option {:?} must declare a known draft method",
                model.model_id,
                option.option_id
            ));
        }
    }
}

fn validate_intelligence_score(score: &IntelligenceScore, errors: &mut Vec<anyhow::Error>) {
    if !score.overall.is_finite() || !(0.0..=100.0).contains(&score.overall) {
        errors.push(anyhow::anyhow!(
            "intelligence score {} must be finite and within 0 to 100",
            score.overall
        ));
    }
}

/// Checks the licence identifier shape and terms link. Membership in the SPDX list is
/// deliberately not checked, so no maintained dataset is needed.
fn validate_license(model_id: &str, license: &License, errors: &mut Vec<anyhow::Error>) {
    let id = license.id.as_str();
    let needs_terms_url = if id == CUSTOM_LICENSE_ID {
        true
    } else if id.eq_ignore_ascii_case(CUSTOM_LICENSE_ID) {
        errors.push(anyhow::anyhow!(
            "model {model_id:?} license id {id:?} must be spelled {CUSTOM_LICENSE_ID:?} in lowercase"
        ));
        false
    } else if valid_spdx_style_id(id) {
        id.starts_with(LICENSE_REF_PREFIX)
    } else {
        errors.push(anyhow::anyhow!(
            "model {model_id:?} license id {id:?} must be an SPDX-style identifier (letters and digits separated by single '.', '-' or '+') or {CUSTOM_LICENSE_ID:?}"
        ));
        false
    };

    if license.terms_url.is_empty() {
        if needs_terms_url {
            errors.push(anyhow::anyhow!(
                "model {model_id:?} license id {id:?} requires a terms URL"
            ));
        }
    } else if !valid_http_url(&license.terms_url) {
        errors.push(anyhow::anyhow!(
            "model {model_id:?} license terms URL {:?} must be an http(s) URL with a host",
            license.terms_url
        ));
    }
}

/// Accepts ASCII letter-and-digit segments joined by single `.`, `-`, or `+` separators.
fn valid_spdx_style_id(id: &str) -> bool {
    let mut previous_separator = true;
    for character in id.chars() {
        if character.is_ascii_alphanumeric() {
            previous_separator = false;
        } else if matches!(character, '.' | '-' | '+') && !previous_separator {
            previous_separator = true;
        } else {
            return false;
        }
    }
    !previous_separator
}

fn valid_http_url(value: &str) -> bool {
    url::Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

fn validate_configurations(
    model: &Model,
    artifacts: &BTreeSet<String>,
    auxiliary: &BTreeSet<String>,
    engines: &BTreeMap<String, EngineRelease>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
    engine_validator: &dyn EngineDeclarationValidator,
    errors: &mut Vec<anyhow::Error>,
) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if model.configurations.is_empty() {
        errors.push(anyhow::anyhow!(
            "model must contain at least one engine configuration"
        ));
    }
    for configuration in &model.configurations {
        if !ids.insert(configuration.configuration_id.clone())
            || !valid_id(&configuration.configuration_id)
        {
            errors.push(anyhow::anyhow!(
                "engine configuration has an invalid or duplicate ID"
            ));
        }
        validate_configuration(
            model,
            configuration,
            artifacts,
            auxiliary,
            engines,
            hardware_classes,
            engine_validator,
            errors,
        );
    }
    ids
}

#[allow(clippy::too_many_arguments)]
fn validate_configuration(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    artifacts: &BTreeSet<String>,
    auxiliary: &BTreeSet<String>,
    engines: &BTreeMap<String, EngineRelease>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
    engine_validator: &dyn EngineDeclarationValidator,
    errors: &mut Vec<anyhow::Error>,
) {
    if configuration.engine_artifacts.is_empty() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires at least one approved engine artifact",
            configuration.configuration_id
        ));
        return;
    }

    let mut references = BTreeSet::new();
    if configuration.engine_artifacts.iter().any(|reference| {
        !valid_id(&reference.engine_artifact_id)
            || !references.insert((
                reference.engine_release_id.as_str(),
                reference.engine_artifact_id.as_str(),
            ))
    }) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has an invalid or duplicate approved engine artifact",
            configuration.configuration_id
        ));
    }

    let Some(engine) = validate_approved_engine_releases(configuration, engines, errors) else {
        return;
    };
    let selected_artifacts = configuration
        .engine_artifacts
        .iter()
        .filter_map(|reference| {
            engines
                .get(&reference.engine_release_id)?
                .artifacts
                .iter()
                .find(|artifact| artifact.artifact_id == reference.engine_artifact_id)
        })
        .collect::<Vec<_>>();
    if selected_artifacts.len() != configuration.engine_artifacts.len() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} references a missing engine artifact",
            configuration.configuration_id
        ));
    }
    if let Some((first, second)) = overlapping_artifacts(&selected_artifacts) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} approves engine artifacts {:?} and {:?} whose selectors can match the same target",
            configuration.configuration_id,
            first,
            second
        ));
    }
    if configuration
        .engine_configuration
        .as_ref()
        .is_none_or(|payload| payload.engine_kind != engine.engine_kind)
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} references an incompatible engine payload",
            configuration.configuration_id
        ));
    }
    let mut binding_ids = BTreeSet::new();
    if configuration.artifact_bindings.is_empty() {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires artifact bindings",
            configuration.configuration_id
        ));
    }
    for binding in &configuration.artifact_bindings {
        let binding_auxiliary_ids = binding
            .auxiliary_artifact_ids
            .iter()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if !binding_ids.insert(binding.binding_id.clone())
            || !valid_id(&binding.binding_id)
            || !artifacts.contains(&binding.model_artifact_id)
            || binding_auxiliary_ids.len() != binding.auxiliary_artifact_ids.len()
            || binding_auxiliary_ids
                .iter()
                .any(|id| !auxiliary.contains(*id))
        {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has an invalid artifact binding",
                configuration.configuration_id
            ));
        }
    }
    if configuration.hardware_support.is_empty()
        || configuration
            .execution_capabilities
            .as_ref()
            .is_none_or(|value| {
                value.supported_parallel_shapes.is_empty()
                    || value.supported_parallel_shapes.contains(&0)
            })
        || !valid_input_capabilities(configuration.input_capabilities.as_ref())
    {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires complete fixed launch inputs",
            configuration.configuration_id
        ));
    }
    if !valid_request_kinds(&configuration.request_kinds) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} requires a non-empty list of unique, known request kinds",
            configuration.configuration_id
        ));
    }
    let supported_classes =
        validate_hardware_support(model, configuration, hardware_classes, errors);
    validate_class_artifact_coverage(
        model,
        configuration,
        &supported_classes,
        &selected_artifacts,
        errors,
    );
    validate_configuration_options(model, configuration, &binding_ids, errors);
    errors.extend(engine_validator.validate_model_configuration(
        model,
        configuration,
        &supported_classes,
        engine,
    ));
}

/// Checks that a configuration's approved releases exist and share one engine kind and one
/// parameter schema with identical definitions, then returns the release standing for all of
/// them. Engine payload, profile and sizing rules read only the kind and the parameters, so they
/// apply equally to every approved release.
fn validate_approved_engine_releases<'a>(
    configuration: &ModelEngineConfiguration,
    engines: &'a BTreeMap<String, EngineRelease>,
    errors: &mut Vec<anyhow::Error>,
) -> Option<&'a EngineRelease> {
    let mut releases: Vec<&EngineRelease> = Vec::new();
    let mut consistent = true;
    for reference in &configuration.engine_artifacts {
        let release_id = reference.engine_release_id.as_str();
        if releases
            .iter()
            .any(|release| release.engine_release_id == release_id)
        {
            continue;
        }
        let Some(release) = engines.get(release_id) else {
            consistent = false;
            errors.push(anyhow::anyhow!(
                "configuration {:?} references a missing engine release {release_id:?}",
                configuration.configuration_id
            ));
            continue;
        };
        if let Some(first) = releases.first() {
            if release.engine_kind != first.engine_kind {
                consistent = false;
                errors.push(anyhow::anyhow!(
                    "configuration {:?} approves engine releases {:?} and {:?} of different engine kinds",
                    configuration.configuration_id,
                    first.engine_release_id,
                    release.engine_release_id
                ));
            } else if !same_parameter_definitions(first, release) {
                consistent = false;
                errors.push(anyhow::anyhow!(
                    "configuration {:?} approves engine releases {:?} and {:?} with different parameter definitions",
                    configuration.configuration_id,
                    first.engine_release_id,
                    release.engine_release_id
                ));
            }
        }
        releases.push(release);
    }
    releases.first().copied().filter(|_| consistent)
}

/// Whether two engine releases declare the same parameter schema id and the same parameter
/// definitions, in any order.
fn same_parameter_definitions(left: &EngineRelease, right: &EngineRelease) -> bool {
    match (
        left.parameter_schema.as_ref(),
        right.parameter_schema.as_ref(),
    ) {
        (Some(left), Some(right)) => {
            left.schema_id == right.schema_id
                && left.parameters.len() == right.parameters.len()
                && left
                    .parameters
                    .iter()
                    .all(|parameter| right.parameters.contains(parameter))
        }
        (None, None) => true,
        _ => false,
    }
}

/// Returns the first two approved artifacts whose selectors can match the same target. Artifact
/// selection requires exactly one match, so even nested selectors conflict.
fn overlapping_artifacts<'a>(artifacts: &[&'a EngineArtifact]) -> Option<(&'a str, &'a str)> {
    artifacts.iter().enumerate().find_map(|(index, first)| {
        artifacts[index + 1..].iter().find_map(|second| {
            first
                .selectors
                .iter()
                .any(|left| {
                    second
                        .selectors
                        .iter()
                        .any(|right| selector_relation(left, right).can_match_same_target())
                })
                .then_some((first.artifact_id.as_str(), second.artifact_id.as_str()))
        })
    })
}

/// Validates a configuration's hardware support and returns the classes it references, in entry
/// order. Each entry references a known class at most once, and supported classes are disjoint or
/// strictly nested, so the most specific match is always unique.
fn validate_hardware_support<'a>(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    hardware_classes: &'a BTreeMap<String, HardwareClass>,
    errors: &mut Vec<anyhow::Error>,
) -> Vec<&'a HardwareClass> {
    let mut supported: Vec<&HardwareClass> = Vec::new();
    for support in &configuration.hardware_support {
        let hardware_class_id = &support.hardware_class_id;
        let Some(hardware_class) = hardware_classes.get(hardware_class_id) else {
            errors.push(anyhow::anyhow!(
                "model {:?} configuration {:?} references unknown hardware class {hardware_class_id:?}",
                model.model_id,
                configuration.configuration_id
            ));
            continue;
        };
        if supported
            .iter()
            .any(|known| &known.hardware_class_id == hardware_class_id)
        {
            errors.push(anyhow::anyhow!(
                "model {:?} configuration {:?} supports hardware class {hardware_class_id:?} more than once",
                model.model_id,
                configuration.configuration_id
            ));
            continue;
        }
        supported.push(hardware_class);
    }
    if let Some((first, second)) = non_nested_classes(&supported) {
        errors.push(anyhow::anyhow!(
            "model {:?} configuration {:?} supports hardware classes {first:?} and {second:?}, whose selectors overlap without one nesting in the other",
            model.model_id,
            configuration.configuration_id
        ));
    }
    supported
}

/// Checks that a configuration's supported hardware classes and approved artifacts cover each
/// other: every class can match a target some approved artifact runs on, and every approved
/// artifact can run on a target some class describes. A class without an artifact could never be
/// launched, and an artifact without a class could never be sized or admitted.
fn validate_class_artifact_coverage(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    classes: &[&HardwareClass],
    artifacts: &[&EngineArtifact],
    errors: &mut Vec<anyhow::Error>,
) {
    // A configuration without supported classes is already rejected for incomplete launch inputs.
    if classes.is_empty() {
        return;
    }

    let can_match_same_target = |class: &HardwareClass, artifact: &EngineArtifact| {
        class.selector.as_ref().is_some_and(|class_selector| {
            artifact.selectors.iter().any(|artifact_selector| {
                selector_relation(class_selector, artifact_selector).can_match_same_target()
            })
        })
    };
    let artifact_ids = artifacts
        .iter()
        .map(|artifact| artifact.artifact_id.as_str())
        .collect::<Vec<_>>();
    let class_ids = classes
        .iter()
        .map(|class| class.hardware_class_id.as_str())
        .collect::<Vec<_>>();

    for class in classes.iter().filter(|class| class.selector.is_some()) {
        if !artifacts
            .iter()
            .any(|artifact| can_match_same_target(class, artifact))
        {
            errors.push(anyhow::anyhow!(
                "model {:?} configuration {:?} supports hardware class {:?}, which no approved engine artifact {artifact_ids:?} can run on",
                model.model_id,
                configuration.configuration_id,
                class.hardware_class_id
            ));
        }
    }
    for artifact in artifacts {
        if !classes
            .iter()
            .any(|class| can_match_same_target(class, artifact))
        {
            errors.push(anyhow::anyhow!(
                "model {:?} configuration {:?} approves engine artifact {:?}, which no supported hardware class {class_ids:?} describes",
                model.model_id,
                configuration.configuration_id,
                artifact.artifact_id
            ));
        }
    }
}

/// Returns the first two classes whose selectors can match one target without one being strictly
/// more specific, which would leave the most specific match ambiguous.
pub(super) fn non_nested_classes<'a>(classes: &[&'a HardwareClass]) -> Option<(&'a str, &'a str)> {
    classes.iter().enumerate().find_map(|(index, first)| {
        classes[index + 1..].iter().find_map(|second| {
            let relation = match (first.selector.as_ref(), second.selector.as_ref()) {
                (Some(left), Some(right)) => selector_relation(left, right),
                _ => SelectorRelation::Disjoint,
            };
            matches!(
                relation,
                SelectorRelation::Equal | SelectorRelation::Overlapping
            )
            .then_some((
                first.hardware_class_id.as_str(),
                second.hardware_class_id.as_str(),
            ))
        })
    })
}

fn validate_configuration_options(
    model: &Model,
    configuration: &ModelEngineConfiguration,
    binding_ids: &BTreeSet<String>,
    errors: &mut Vec<anyhow::Error>,
) {
    let mut option_ids = BTreeSet::new();
    for option in &configuration.options {
        let model_option = model
            .options
            .iter()
            .find(|candidate| candidate.option_id == option.option_id);
        if !option_ids.insert(option.option_id.clone())
            || model_option.is_none()
            || !valid_configuration_option(option, binding_ids)
        {
            errors.push(anyhow::anyhow!(
                "configuration {:?} has invalid option {:?}",
                configuration.configuration_id,
                option.option_id
            ));
        }
        match model_option.and_then(|option| option.kind.as_ref()) {
            Some(ModelOptionKind::SpeculativeDecoding(_)) => {
                if let Some(problem) = speculative_parameter_problem(&option.parameters) {
                    errors.push(anyhow::anyhow!(
                        "model {:?} configuration {:?} speculative decoding option {:?} {problem}",
                        model.model_id,
                        configuration.configuration_id,
                        option.option_id
                    ));
                }
            }
            Some(ModelOptionKind::KvCachePrecision(_)) => {
                if let Some(problem) = kv_cache_precision_parameter_problem(&option.parameters) {
                    errors.push(anyhow::anyhow!(
                        "model {:?} configuration {:?} KV cache precision option {:?} {problem}",
                        model.model_id,
                        configuration.configuration_id,
                        option.option_id
                    ));
                }
            }
            Some(ModelOptionKind::Vision(_)) | None => {}
        }
    }
    let default_values = configuration
        .options
        .iter()
        .filter_map(|option| option.default_values.clone())
        .collect::<Vec<_>>();
    if let Err(error) = reject_multiple_active_speculative_options(&model.options, &default_values)
    {
        errors.push(anyhow::anyhow!(
            "model {:?} configuration {:?} has invalid option defaults: {error}",
            model.model_id,
            configuration.configuration_id
        ));
    }
    if let Err(error) = crate::sizing::validate_compatibility(configuration) {
        errors.push(anyhow::anyhow!(
            "configuration {:?} has invalid option compatibility: {error}",
            configuration.configuration_id
        ));
    }
}

fn valid_configuration_option(
    option: &ModelConfigurationOption,
    binding_ids: &BTreeSet<String>,
) -> bool {
    let mut parameter_ids = BTreeSet::new();
    let parameters_valid = !option.parameters.is_empty()
        && option.parameters.iter().all(|parameter| {
            parameter_ids.insert(parameter.parameter_id.clone())
                && valid_id(&parameter.parameter_id)
                && !parameter.display_name.is_empty()
                && valid_option_parameter(parameter)
        });
    let supported_bindings = option.supported_binding_ids.iter().collect::<BTreeSet<_>>();
    parameters_valid
        && option
            .default_values
            .as_ref()
            .is_some_and(|values| valid_option_values(values, option, true))
        && supported_bindings.len() == option.supported_binding_ids.len()
        && supported_bindings
            .iter()
            .all(|id| binding_ids.contains(*id))
}

/// Describes how a speculative-decoding option's parameters depart from the fixed shape: a
/// single speculative-token count from 0 (off) to at least 1, counted in tokens.
fn speculative_parameter_problem(parameters: &[ModelOptionParameterDefinition]) -> Option<String> {
    let [parameter] = parameters else {
        return Some(format!(
            "must declare exactly one parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?}"
        ));
    };
    if parameter.parameter_id != SPECULATIVE_TOKEN_COUNT_PARAMETER_ID {
        return Some(format!(
            "must name its parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?}"
        ));
    }
    let Some(ModelOptionParameterType::UnsignedInteger(range)) = parameter.parameter_type.as_ref()
    else {
        return Some(format!(
            "parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?} must be an unsigned integer"
        ));
    };
    if range.minimum != 0 {
        return Some(format!(
            "parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?} must have minimum 0"
        ));
    }
    if range.maximum < 1 {
        return Some(format!(
            "parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?} must have maximum at least 1"
        ));
    }
    if range.unit != SPECULATIVE_TOKEN_COUNT_UNIT {
        return Some(format!(
            "parameter {SPECULATIVE_TOKEN_COUNT_PARAMETER_ID:?} must use unit {SPECULATIVE_TOKEN_COUNT_UNIT:?}"
        ));
    }
    None
}

/// Describes how a KV cache precision option's parameters depart from the fixed shape: a single
/// string-enum precision. The generic checks require at least one value and a default among them.
fn kv_cache_precision_parameter_problem(
    parameters: &[ModelOptionParameterDefinition],
) -> Option<String> {
    let [parameter] = parameters else {
        return Some(format!(
            "must declare exactly one parameter {KV_CACHE_PRECISION_PARAMETER_ID:?}"
        ));
    };
    if parameter.parameter_id != KV_CACHE_PRECISION_PARAMETER_ID {
        return Some(format!(
            "must name its parameter {KV_CACHE_PRECISION_PARAMETER_ID:?}"
        ));
    }
    if !matches!(
        parameter.parameter_type,
        Some(ModelOptionParameterType::StringEnum(_))
    ) {
        return Some(format!(
            "parameter {KV_CACHE_PRECISION_PARAMETER_ID:?} must be a string enum"
        ));
    }
    None
}

fn valid_option_parameter(parameter: &ModelOptionParameterDefinition) -> bool {
    match parameter.parameter_type.as_ref() {
        Some(ModelOptionParameterType::Boolean(_)) => true,
        Some(ModelOptionParameterType::UnsignedInteger(range)) => range.minimum <= range.maximum,
        Some(ModelOptionParameterType::StringEnum(schema)) => {
            let values = schema.values.iter().collect::<BTreeSet<_>>();
            !schema.values.is_empty()
                && values.len() == schema.values.len()
                && values.iter().all(|value| {
                    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
                })
        }
        None => false,
    }
}

fn validate_profiles(
    model: &Model,
    configurations: &BTreeSet<String>,
    engines: &BTreeMap<String, EngineRelease>,
    errors: &mut Vec<anyhow::Error>,
) {
    let mut ids = BTreeSet::new();
    if model.profiles.is_empty() {
        errors.push(anyhow::anyhow!(
            "model must contain at least one nested profile"
        ));
    }
    for profile in &model.profiles {
        if !ids.insert(profile.profile_id.clone())
            || !valid_id(&profile.profile_id)
            || !configurations.contains(&profile.configuration_id)
        {
            errors.push(anyhow::anyhow!(
                "profile {:?} has invalid provenance",
                profile.profile_id
            ));
        }
        let Some(configuration) = model
            .configurations
            .iter()
            .find(|value| value.configuration_id == profile.configuration_id)
        else {
            continue;
        };
        if !configuration
            .artifact_bindings
            .iter()
            .any(|binding| binding.binding_id == profile.default_model_artifact_binding_id)
        {
            errors.push(anyhow::anyhow!(
                "profile {:?} references a missing default artifact binding",
                profile.profile_id
            ));
        }
        let Some(engine) = configuration_engine_release(configuration, engines) else {
            continue;
        };
        validate_visibility_overrides(profile, engine, errors);
        match profile.engine_parameter_defaults.as_ref() {
            Some(values) => {
                validate_supplied_parameter_values(values, engine, &profile.profile_id, errors);
                for value in values
                    .values
                    .iter()
                    .filter(|value| !profile_may_default(&value.parameter_id))
                {
                    errors.push(anyhow::anyhow!(
                        "profile {:?} sets engine parameter {:?}, which profiles leave to the engine default and the deployment",
                        profile.profile_id,
                        value.parameter_id
                    ));
                }
                // A secret default would be a literal every deployment shares.
                for value in values
                    .values
                    .iter()
                    .filter(|value| is_secret_parameter(engine, &value.parameter_id))
                {
                    errors.push(anyhow::anyhow!(
                        "profile {:?} sets secret engine parameter {:?}, which each deployment generates or receives",
                        profile.profile_id,
                        value.parameter_id
                    ));
                }
            }
            None => errors.push(anyhow::anyhow!(
                "profile {:?} requires engine parameter defaults",
                profile.profile_id
            )),
        }
        validate_profile_option_defaults(profile, configuration, errors);
        if let Err(error) = resolve_model_option_values(&model.options, configuration, profile, &[])
        {
            errors.push(anyhow::anyhow!(
                "profile {:?} has invalid resolved model options: {error}",
                profile.profile_id
            ));
        }
    }
}

/// Validates a profile's engine parameter visibility overrides: each names a parameter of the
/// engine release once, with a visibility the shared parameter rules allow for profiles.
fn validate_visibility_overrides(
    profile: &ModelProfile,
    engine: &EngineRelease,
    errors: &mut Vec<anyhow::Error>,
) {
    let profile_id = &profile.profile_id;
    let mut ids = BTreeSet::new();
    for visibility_override in &profile.engine_parameter_visibility_overrides {
        let parameter_id = &visibility_override.parameter_id;
        if !engine
            .parameter_schema
            .iter()
            .flat_map(|schema| &schema.parameters)
            .any(|definition| &definition.parameter_id == parameter_id)
        {
            errors.push(anyhow::anyhow!(
                "profile {profile_id:?} overrides the visibility of engine parameter {parameter_id:?}, which is not a parameter of the engine release"
            ));
            continue;
        }
        if !ids.insert(parameter_id) {
            errors.push(anyhow::anyhow!(
                "profile {profile_id:?} overrides the visibility of engine parameter {parameter_id:?} more than once"
            ));
            continue;
        }
        if let Some(problem) =
            profile_visibility_problem(parameter_id, visibility_override.visibility())
        {
            errors.push(anyhow::anyhow!(
                "profile {profile_id:?} engine parameter {parameter_id:?} {problem}"
            ));
        }
    }
}

fn validate_profile_option_defaults(
    profile: &dotcore_proto::dotcore::catalog::v1::ModelProfile,
    configuration: &ModelEngineConfiguration,
    errors: &mut Vec<anyhow::Error>,
) {
    let mut ids = BTreeSet::new();
    for values in &profile.option_defaults {
        let option = configuration
            .options
            .iter()
            .find(|option| option.option_id == values.option_id);
        if !ids.insert(values.option_id.clone())
            || option.is_none_or(|option| !valid_option_values(values, option, false))
        {
            errors.push(anyhow::anyhow!(
                "profile {:?} has invalid option defaults",
                profile.profile_id
            ));
        }
    }
}

/// Validates a complete set of engine parameter values against the engine schema.
pub fn validate_parameter_values(
    values: &ParameterValues,
    engine: &EngineRelease,
    profile_id: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    validate_values(
        values,
        engine,
        profile_id,
        SecretCompleteness::Required,
        errors,
    );
}

/// Validates engine parameter values that are complete except, possibly, for secrets, whose
/// values exist only once a deployment is created.
pub fn validate_parameter_values_before_secrets(
    values: &ParameterValues,
    engine: &EngineRelease,
    profile_id: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    validate_values(
        values,
        engine,
        profile_id,
        SecretCompleteness::Optional,
        errors,
    );
}

/// Whether the engine release declares `parameter_id` as a secret.
pub fn is_secret_parameter(engine: &EngineRelease, parameter_id: &str) -> bool {
    engine
        .parameter_schema
        .iter()
        .flat_map(|schema| &schema.parameters)
        .any(|definition| definition.parameter_id == parameter_id && is_secret(definition))
}

/// Validates supplied engine parameter values, which may omit parameters that sizing derives.
/// Completeness is checked once derivations are known.
pub fn validate_supplied_parameter_values(
    values: &ParameterValues,
    engine: &EngineRelease,
    profile_id: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(schema) = engine.parameter_schema.as_ref() else {
        return;
    };
    let mut ids = BTreeSet::new();
    for value in &values.values {
        let parameter_id = &value.parameter_id;
        let Some(definition) = schema
            .parameters
            .iter()
            .find(|definition| &definition.parameter_id == parameter_id)
        else {
            errors.push(anyhow::anyhow!(
                "profile {profile_id:?} engine parameter {parameter_id:?} is not a parameter of the engine release"
            ));
            continue;
        };
        if !ids.insert(parameter_id.clone()) {
            errors.push(anyhow::anyhow!(
                "profile {profile_id:?} engine parameter {parameter_id:?} has more than one value"
            ));
            continue;
        }
        match (definition.parameter_type.as_ref(), value.value.as_ref()) {
            (
                Some(EngineParameterType::UnsignedInteger(range)),
                Some(EngineParameterValueKind::UnsignedInteger(value)),
            ) => {
                if !(range.minimum..=range.maximum).contains(value) {
                    errors.push(anyhow::anyhow!(
                        "profile {profile_id:?} engine parameter {parameter_id:?} value {value} is outside its range {}..={}",
                        range.minimum,
                        range.maximum
                    ));
                }
            }
            (Some(EngineParameterType::Boolean(_)), Some(EngineParameterValueKind::Boolean(_))) => {
            }
            (Some(EngineParameterType::Secret(_)), Some(value @ EngineParameterValueKind::String(_))) => {
                if let Err(error) = check_value(definition, value) {
                    errors.push(anyhow::anyhow!(
                        "profile {profile_id:?} engine parameter {parameter_id:?} value is invalid: {error}"
                    ));
                }
            }
            _ => errors.push(anyhow::anyhow!(
                "profile {profile_id:?} engine parameter {parameter_id:?} has a value of the wrong type"
            )),
        }
    }
}

/// Overlays defaults on supplied engine parameter values, per parameter: the supplied values
/// keep their order, followed by each default whose parameter they leave out.
pub fn overlay_parameter_defaults(
    values: &ParameterValues,
    defaults: &ParameterValues,
) -> ParameterValues {
    let mut overlaid = values.clone();
    overlaid.values.extend(
        defaults
            .values
            .iter()
            .filter(|default| {
                !values
                    .values
                    .iter()
                    .any(|value| value.parameter_id == default.parameter_id)
            })
            .cloned(),
    );
    overlaid
}

/// Engine defaults the release declares for its parameters, in schema order.
pub(super) fn engine_default_values(engine: &EngineRelease) -> ParameterValues {
    ParameterValues {
        values: engine
            .parameter_schema
            .iter()
            .flat_map(|schema| &schema.parameters)
            .filter_map(|definition| {
                default_value(definition).map(|default| ParameterValue {
                    parameter_id: definition.parameter_id.clone(),
                    value: Some(default),
                })
            })
            .collect(),
    }
}

/// Engine schema parameter ids without a supplied value, in schema order.
pub fn missing_parameter_ids(values: &ParameterValues, engine: &EngineRelease) -> Vec<String> {
    engine
        .parameter_schema
        .as_ref()
        .map(|schema| {
            schema
                .parameters
                .iter()
                .filter(|definition| {
                    !values
                        .values
                        .iter()
                        .any(|value| value.parameter_id == definition.parameter_id)
                })
                .map(|definition| definition.parameter_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Whether value completeness covers secret parameters.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SecretCompleteness {
    Required,
    Optional,
}

fn validate_values(
    values: &ParameterValues,
    engine: &EngineRelease,
    profile_id: &str,
    secrets: SecretCompleteness,
    errors: &mut Vec<anyhow::Error>,
) {
    let missing = missing_parameter_ids(values, engine)
        .into_iter()
        .filter(|parameter_id| {
            secrets == SecretCompleteness::Required || !is_secret_parameter(engine, parameter_id)
        })
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        errors.push(anyhow::anyhow!(
            "profile {profile_id:?} engine parameter values are incomplete; missing {}",
            missing.join(", ")
        ));
        return;
    }
    validate_supplied_parameter_values(values, engine, profile_id, errors);
}

fn valid_weight_quantization(value: &str) -> bool {
    value.is_empty()
        || (value.len() <= MAX_WEIGHT_QUANTIZATION_BYTES
            && !value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control()))
}

fn valid_input_capabilities(capabilities: Option<&ModelInputCapabilities>) -> bool {
    capabilities.is_some_and(|capabilities| {
        capabilities
            .supported_modalities
            .contains(&(ModelInputModality::Text as i32))
            && capabilities.supported_modalities.iter().all(|value| {
                matches!(
                    ModelInputModality::try_from(*value),
                    Ok(ModelInputModality::Text | ModelInputModality::Image)
                )
            })
            && (capabilities
                .supported_modalities
                .contains(&(ModelInputModality::Image as i32))
                == capabilities.image_input_capacity.is_some())
    })
}

fn valid_request_kinds(kinds: &[i32]) -> bool {
    !kinds.is_empty()
        && kinds.iter().all(|value| {
            matches!(
                InferenceRequestKind::try_from(*value),
                Ok(InferenceRequestKind::Chat | InferenceRequestKind::Decision)
            )
        })
        && kinds.iter().collect::<BTreeSet<_>>().len() == kinds.len()
}

pub fn valid_pinned_hugging_face_artifact(artifact: &HuggingFaceArtifactSource) -> bool {
    let mut repository = artifact.repository.split('/');
    let coordinates_valid = repository.next().is_some_and(valid_repository_component)
        && repository.next().is_some_and(valid_repository_component)
        && repository.next().is_none()
        && lower_hex(&artifact.revision, 40);
    coordinates_valid
        && match artifact.content.as_ref() {
            Some(HuggingFaceContent::File(file)) => {
                valid_hugging_face_repository_path(&file.path)
                    && (file.provenance_digest.is_empty() || lower_hex(&file.provenance_digest, 64))
            }
            Some(HuggingFaceContent::Snapshot(snapshot)) => {
                valid_hugging_face_snapshot(&snapshot.files)
            }
            None => false,
        }
}

fn valid_hugging_face_snapshot(
    files: &[dotcore_proto::dotcore::catalog::v1::HuggingFaceSnapshotFile],
) -> bool {
    let paths = files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    !files.is_empty()
        && paths.len() == files.len()
        && files.iter().all(|file| {
            valid_hugging_face_repository_path(&file.path) && lower_hex(&file.provenance_digest, 64)
        })
        && paths.iter().all(|path| {
            let mut ancestor = String::new();
            let segments = path.split('/').collect::<Vec<_>>();
            segments[..segments.len() - 1].iter().all(|segment| {
                if !ancestor.is_empty() {
                    ancestor.push('/');
                }
                ancestor.push_str(segment);
                !paths.contains(ancestor.as_str())
            })
        })
}

fn valid_hugging_face_repository_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\\')
        && path.split('/').all(valid_hugging_face_path_segment)
}

fn valid_hugging_face_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && !matches!(value, "." | "..")
        && !value.chars().any(char::is_control)
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains("..")
        && value
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use dotcore_proto::dotcore::catalog::v1::{
        BooleanModelOptionParameterSchema, EngineArtifact, EngineArtifactReference, EngineRelease,
        HardwareClass, InferenceRequestKind, KvCachePrecisionModelOption, Model,
        ModelArtifactBinding, ModelConfigurationOption, ModelEngineConfiguration, ModelOption,
        ModelOptionParameterDefinition, ModelOptionParameterValue, ModelOptionValues, ModelProfile,
        OciPlatform, ParameterValues, Selector, SpeculativeDecodingMethod,
        SpeculativeDecodingModelOption, StringEnumModelOptionParameterSchema,
        UnsignedIntegerModelOptionParameterSchema, model_option,
        model_option_parameter_definition::ParameterType, model_option_parameter_value::Value,
    };

    use super::{
        engine_default_values, overlapping_artifacts, valid_request_kinds,
        validate_class_artifact_coverage, validate_configuration_options, validate_model_options,
        validate_profiles,
    };
    use crate::test_support::vendor_hardware_class;

    #[test]
    fn engine_defaults_cover_every_parameter_declaring_one_booleans_included() {
        use dotcore_proto::dotcore::catalog::v1::{
            BooleanParameterSchema, EngineParameterSchema, ParameterDefinition, ParameterValue,
            UnsignedIntegerParameterSchema, parameter_definition::ParameterType as Type,
            parameter_value::Value as ParameterValueKind,
        };

        let definition = |parameter_id: &str, parameter_type| ParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            parameter_type: Some(parameter_type),
            ..Default::default()
        };
        let unsigned = |default_value| {
            Type::UnsignedInteger(UnsignedIntegerParameterSchema {
                minimum: 0,
                maximum: 64,
                unit: "requests".to_owned(),
                default_value,
            })
        };
        let boolean = |default_value| Type::Boolean(BooleanParameterSchema { default_value });
        let engine = EngineRelease {
            parameter_schema: Some(EngineParameterSchema {
                schema_id: "fixture".to_owned(),
                parameters: vec![
                    definition("parallel-requests", unsigned(Some(4))),
                    definition("flash-attention", boolean(Some(false))),
                    definition("cache-mib", unsigned(None)),
                    definition("mlock", boolean(None)),
                ],
            }),
            ..Default::default()
        };
        let value = |parameter_id: &str, value| ParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(value),
        };

        assert_eq!(
            engine_default_values(&engine),
            ParameterValues {
                values: vec![
                    value("parallel-requests", ParameterValueKind::UnsignedInteger(4)),
                    value("flash-attention", ParameterValueKind::Boolean(false)),
                ],
            }
        );
    }

    #[test]
    fn request_kinds_must_be_non_empty_known_and_unique() {
        let chat = InferenceRequestKind::Chat as i32;
        let decision = InferenceRequestKind::Decision as i32;

        assert!(valid_request_kinds(&[chat]));
        assert!(valid_request_kinds(&[chat, decision]));
        assert!(!valid_request_kinds(&[]));
        assert!(!valid_request_kinds(&[chat, chat]));
        assert!(!valid_request_kinds(&[
            InferenceRequestKind::Unspecified as i32
        ]));
        assert!(!valid_request_kinds(&[chat, 99]));
    }

    #[test]
    fn approved_artifacts_overlap_when_any_selectors_can_match_one_target() {
        let linux = |architecture: &str, accelerators: &[&str]| Selector {
            platform: Some(OciPlatform {
                operating_system: "linux".to_owned(),
                architecture: architecture.to_owned(),
                variant: None,
            }),
            accelerators: accelerators
                .iter()
                .map(|&vendor| vendor.to_owned())
                .collect(),
            minimum_nvidia_driver_major_version: 580,
            ..Default::default()
        };
        let artifact = |artifact_id: &str, selectors: Vec<Selector>| EngineArtifact {
            artifact_id: artifact_id.to_owned(),
            selectors,
            ..Default::default()
        };
        let amd64 = artifact("amd64", vec![linux("amd64", &["nvidia"])]);
        let arm64 = artifact("arm64", vec![linux("arm64", &["nvidia"])]);
        let equal = artifact("equal", vec![linux("amd64", &["nvidia"])]);
        let nested = artifact("nested", vec![linux("arm64", &["nvidia", "apple"])]);
        let any_platform = artifact(
            "any-platform",
            vec![Selector {
                platform: None,
                ..linux("amd64", &["apple"])
            }],
        );

        assert_eq!(overlapping_artifacts(&[&amd64, &arm64]), None);
        assert_eq!(
            overlapping_artifacts(&[&amd64, &arm64, &equal]),
            Some(("amd64", "equal"))
        );
        assert_eq!(
            overlapping_artifacts(&[&amd64, &arm64, &nested]),
            Some(("arm64", "nested"))
        );
        assert_eq!(overlapping_artifacts(&[&amd64, &any_platform]), None);
        assert_eq!(
            overlapping_artifacts(&[&nested, &any_platform]),
            Some(("nested", "any-platform"))
        );
    }

    #[test]
    fn supported_classes_and_approved_artifacts_must_cover_each_other() {
        let artifact =
            |artifact_id: &str, operating_system: &str, accelerator: &str| EngineArtifact {
                artifact_id: artifact_id.to_owned(),
                selectors: vec![Selector {
                    platform: Some(OciPlatform {
                        operating_system: operating_system.to_owned(),
                        architecture: "arm64".to_owned(),
                        variant: None,
                    }),
                    accelerators: vec![accelerator.to_owned()],
                    ..Default::default()
                }],
                ..Default::default()
            };
        let cuda = artifact("cuda", "linux", "nvidia");
        let metal = artifact("metal", "darwin", "apple");
        let nvidia = vendor_hardware_class("nvidia");
        let apple = vendor_hardware_class("apple");
        let errors = |classes: &[&HardwareClass], artifacts: &[&EngineArtifact]| {
            let model = Model {
                model_id: "fixture-model".to_owned(),
                ..Default::default()
            };
            let configuration = ModelEngineConfiguration {
                configuration_id: "fixture-configuration".to_owned(),
                ..Default::default()
            };
            let mut errors = Vec::new();
            validate_class_artifact_coverage(
                &model,
                &configuration,
                classes,
                artifacts,
                &mut errors,
            );
            errors.iter().map(ToString::to_string).collect::<Vec<_>>()
        };

        assert_eq!(
            errors(&[&nvidia, &apple], &[&cuda, &metal]),
            Vec::<String>::new()
        );
        assert_eq!(
            errors(&[&nvidia, &apple], &[&cuda]),
            [
                "model \"fixture-model\" configuration \"fixture-configuration\" supports hardware class \"apple\", which no approved engine artifact [\"cuda\"] can run on"
            ]
        );
        assert_eq!(
            errors(&[&nvidia], &[&cuda, &metal]),
            [
                "model \"fixture-model\" configuration \"fixture-configuration\" approves engine artifact \"metal\", which no supported hardware class [\"nvidia\"] describes"
            ]
        );
    }

    #[test]
    fn speculative_options_with_the_fixed_shape_are_accepted() {
        assert_eq!(option_errors(&speculative_model()), Vec::<String>::new());
    }

    #[test]
    fn speculative_option_without_a_known_draft_method_is_rejected() {
        for method in [SpeculativeDecodingMethod::Unspecified as i32, i32::MAX] {
            let mut model = speculative_model();
            model.options[0].kind = Some(model_option::Kind::SpeculativeDecoding(
                SpeculativeDecodingModelOption { method },
            ));

            assert_eq!(
                option_errors(&model),
                vec![
                    "model \"fixture-model\" speculative decoding option \"mtp\" must declare a known draft method"
                ],
                "{method}"
            );
        }
    }

    #[test]
    fn speculative_option_parameters_must_match_the_fixed_shape() {
        let speculative_error = |problem: &str| {
            format!(
                "model \"fixture-model\" configuration \"local\" speculative decoding option \"mtp\" {problem}"
            )
        };
        let cases: [(&str, OptionMutation, Vec<String>); 7] = [
            (
                "missing parameter",
                |option| {
                    option.parameters.clear();
                    option.default_values = Some(ModelOptionValues {
                        option_id: "mtp".to_owned(),
                        values: Vec::new(),
                    });
                },
                vec![
                    "configuration \"local\" has invalid option \"mtp\"".to_owned(),
                    speculative_error("must declare exactly one parameter \"token-count\""),
                ],
            ),
            (
                "extra parameter",
                |option| {
                    option
                        .parameters
                        .push(unsigned_parameter("draft-minimum", 0, 8));
                    option
                        .default_values
                        .as_mut()
                        .expect("defaults")
                        .values
                        .push(unsigned_value("draft-minimum", 0));
                },
                vec![speculative_error(
                    "must declare exactly one parameter \"token-count\"",
                )],
            ),
            (
                "renamed parameter",
                |option| {
                    option.parameters = vec![unsigned_parameter("draft-maximum", 0, 8)];
                    option.default_values = Some(ModelOptionValues {
                        option_id: "mtp".to_owned(),
                        values: vec![unsigned_value("draft-maximum", 0)],
                    });
                },
                vec![speculative_error("must name its parameter \"token-count\"")],
            ),
            (
                "boolean parameter",
                |option| {
                    option.parameters[0].parameter_type =
                        Some(ParameterType::Boolean(BooleanModelOptionParameterSchema {}));
                    option.default_values = Some(ModelOptionValues {
                        option_id: "mtp".to_owned(),
                        values: vec![ModelOptionParameterValue {
                            parameter_id: "token-count".to_owned(),
                            value: Some(Value::Boolean(false)),
                        }],
                    });
                },
                vec![speculative_error(
                    "parameter \"token-count\" must be an unsigned integer",
                )],
            ),
            (
                "nonzero minimum",
                |option| {
                    option.parameters = vec![unsigned_parameter("token-count", 1, 8)];
                    option.default_values = Some(token_count("mtp", 1));
                },
                vec![speculative_error(
                    "parameter \"token-count\" must have minimum 0",
                )],
            ),
            (
                "zero maximum",
                |option| option.parameters = vec![unsigned_parameter("token-count", 0, 0)],
                vec![speculative_error(
                    "parameter \"token-count\" must have maximum at least 1",
                )],
            ),
            (
                "unitless parameter",
                |option| {
                    let Some(ParameterType::UnsignedInteger(range)) =
                        option.parameters[0].parameter_type.as_mut()
                    else {
                        unreachable!("fixture token count is unsigned");
                    };
                    range.unit.clear();
                },
                vec![speculative_error(
                    "parameter \"token-count\" must use unit \"tokens\"",
                )],
            ),
        ];

        for (case, mutate, expected) in cases {
            let mut model = speculative_model();
            mutate(&mut model.configurations[0].options[0]);

            assert_eq!(option_errors(&model), expected, "{case}");
        }
    }

    #[test]
    fn configuration_defaults_activating_two_speculative_options_are_rejected() {
        let mut model = speculative_model();
        model.profiles.clear();
        model.configurations[0].options[0].default_values = Some(token_count("mtp", 3));
        model.configurations[0].options[1].default_values = Some(token_count("dflash", 2));

        assert_eq!(
            option_errors(&model),
            vec![
                "model \"fixture-model\" configuration \"local\" has invalid option defaults: at most one speculative decoding option may have a token count above 0, but \"mtp\", \"dflash\" are active"
            ]
        );
    }

    #[test]
    fn profile_defaults_activating_two_speculative_options_are_rejected() {
        let mut model = speculative_model();
        model.profiles[0].option_defaults = vec![token_count("mtp", 3), token_count("dflash", 2)];

        assert_eq!(option_errors(&model), Vec::<String>::new());
        assert_eq!(
            profile_errors(&model),
            vec![
                "profile \"balanced\" has invalid resolved model options: at most one speculative decoding option may have a token count above 0, but \"mtp\", \"dflash\" are active"
            ]
        );
    }

    #[test]
    fn kv_cache_precision_option_with_the_fixed_shape_is_accepted() {
        assert_eq!(option_errors(&kv_cache_model()), Vec::<String>::new());
    }

    #[test]
    fn kv_cache_precision_option_parameters_must_match_the_fixed_shape() {
        let shape_error = |problem: &str| {
            format!(
                "model \"fixture-model\" configuration \"local\" KV cache precision option \"kv-cache\" {problem}"
            )
        };
        let invalid_option = "configuration \"local\" has invalid option \"kv-cache\"".to_owned();
        let cases: [(&str, OptionMutation, Vec<String>); 6] = [
            (
                "missing parameter",
                |option| {
                    option.parameters.clear();
                    option.default_values = Some(ModelOptionValues {
                        option_id: "kv-cache".to_owned(),
                        values: Vec::new(),
                    });
                },
                vec![
                    invalid_option.clone(),
                    shape_error("must declare exactly one parameter \"precision\""),
                ],
            ),
            (
                "extra parameter",
                |option| {
                    option
                        .parameters
                        .push(string_enum_parameter("layout", &["paged"]));
                    option
                        .default_values
                        .as_mut()
                        .expect("defaults")
                        .values
                        .push(string_value("layout", "paged"));
                },
                vec![shape_error(
                    "must declare exactly one parameter \"precision\"",
                )],
            ),
            (
                "renamed parameter",
                |option| {
                    option.parameters = vec![string_enum_parameter("type", &["f16"])];
                    option.default_values = Some(ModelOptionValues {
                        option_id: "kv-cache".to_owned(),
                        values: vec![string_value("type", "f16")],
                    });
                },
                vec![shape_error("must name its parameter \"precision\"")],
            ),
            (
                "boolean parameter",
                |option| {
                    option.parameters[0].parameter_type =
                        Some(ParameterType::Boolean(BooleanModelOptionParameterSchema {}));
                    option.default_values = Some(ModelOptionValues {
                        option_id: "kv-cache".to_owned(),
                        values: vec![ModelOptionParameterValue {
                            parameter_id: "precision".to_owned(),
                            value: Some(Value::Boolean(false)),
                        }],
                    });
                },
                vec![shape_error("parameter \"precision\" must be a string enum")],
            ),
            (
                "no precisions",
                |option| option.parameters = vec![string_enum_parameter("precision", &[])],
                vec![invalid_option.clone()],
            ),
            (
                "default outside the precisions",
                |option| {
                    option.default_values = Some(precision("kv-cache", "q4_0"));
                },
                vec![invalid_option],
            ),
        ];

        for (case, mutate, expected) in cases {
            let mut model = kv_cache_model();
            mutate(&mut model.configurations[0].options[0]);

            assert_eq!(option_errors(&model), expected, "{case}");
        }
    }

    /// Edit applied to a valid speculative configuration option.
    type OptionMutation = fn(&mut ModelConfigurationOption);

    /// Model-level and configuration option errors of a model.
    fn option_errors(model: &Model) -> Vec<String> {
        let mut errors = Vec::new();
        validate_model_options(model, &mut errors);
        validate_configuration_options(
            model,
            &model.configurations[0],
            &BTreeSet::from(["weights".to_owned()]),
            &mut errors,
        );
        errors.iter().map(ToString::to_string).collect()
    }

    /// Profile errors of a model whose configurations use an engine without a parameter schema.
    fn profile_errors(model: &Model) -> Vec<String> {
        let mut errors = Vec::new();
        let engines = BTreeMap::from([(
            "fixture-engine".to_owned(),
            EngineRelease {
                engine_release_id: "fixture-engine".to_owned(),
                ..Default::default()
            },
        )]);
        validate_profiles(
            model,
            &BTreeSet::from(["local".to_owned()]),
            &engines,
            &mut errors,
        );
        errors.iter().map(ToString::to_string).collect()
    }

    /// Model with two speculative options, both off by default in its only configuration.
    fn speculative_model() -> Model {
        let option_ids = ["mtp", "dflash"];
        Model {
            model_id: "fixture-model".to_owned(),
            options: option_ids
                .into_iter()
                .map(|option_id| ModelOption {
                    option_id: option_id.to_owned(),
                    display_name: option_id.to_uppercase(),
                    kind: Some(model_option::Kind::SpeculativeDecoding(
                        SpeculativeDecodingModelOption {
                            method: SpeculativeDecodingMethod::Mtp as i32,
                        },
                    )),
                })
                .collect(),
            configurations: vec![ModelEngineConfiguration {
                configuration_id: "local".to_owned(),
                engine_artifacts: vec![EngineArtifactReference {
                    engine_release_id: "fixture-engine".to_owned(),
                    engine_artifact_id: "artifact".to_owned(),
                }],
                artifact_bindings: vec![ModelArtifactBinding {
                    binding_id: "weights".to_owned(),
                    ..Default::default()
                }],
                options: option_ids
                    .into_iter()
                    .map(|option_id| ModelConfigurationOption {
                        option_id: option_id.to_owned(),
                        parameters: vec![unsigned_parameter("token-count", 0, 8)],
                        default_values: Some(token_count(option_id, 0)),
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            }],
            profiles: vec![ModelProfile {
                profile_id: "balanced".to_owned(),
                configuration_id: "local".to_owned(),
                default_model_artifact_binding_id: "weights".to_owned(),
                engine_parameter_defaults: Some(ParameterValues::default()),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// Model with one KV cache precision option offering f16 and q8_0, f16 by default.
    fn kv_cache_model() -> Model {
        let mut model = speculative_model();
        model.options = vec![ModelOption {
            option_id: "kv-cache".to_owned(),
            display_name: "KV cache precision".to_owned(),
            kind: Some(model_option::Kind::KvCachePrecision(
                KvCachePrecisionModelOption {},
            )),
        }];
        model.configurations[0].options = vec![ModelConfigurationOption {
            option_id: "kv-cache".to_owned(),
            parameters: vec![string_enum_parameter("precision", &["f16", "q8_0"])],
            default_values: Some(precision("kv-cache", "f16")),
            ..Default::default()
        }];
        model
    }

    fn unsigned_parameter(
        parameter_id: &str,
        minimum: u64,
        maximum: u64,
    ) -> ModelOptionParameterDefinition {
        ModelOptionParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            display_name: parameter_id.to_owned(),
            parameter_type: Some(ParameterType::UnsignedInteger(
                UnsignedIntegerModelOptionParameterSchema {
                    minimum,
                    maximum,
                    unit: "tokens".to_owned(),
                },
            )),
        }
    }

    fn token_count(option_id: &str, count: u64) -> ModelOptionValues {
        ModelOptionValues {
            option_id: option_id.to_owned(),
            values: vec![unsigned_value("token-count", count)],
        }
    }

    fn string_enum_parameter(
        parameter_id: &str,
        values: &[&str],
    ) -> ModelOptionParameterDefinition {
        ModelOptionParameterDefinition {
            parameter_id: parameter_id.to_owned(),
            display_name: parameter_id.to_owned(),
            parameter_type: Some(ParameterType::StringEnum(
                StringEnumModelOptionParameterSchema {
                    values: values.iter().map(|&value| value.to_owned()).collect(),
                },
            )),
        }
    }

    fn precision(option_id: &str, value: &str) -> ModelOptionValues {
        ModelOptionValues {
            option_id: option_id.to_owned(),
            values: vec![string_value("precision", value)],
        }
    }

    fn string_value(parameter_id: &str, value: &str) -> ModelOptionParameterValue {
        ModelOptionParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(Value::String(value.to_owned())),
        }
    }

    fn unsigned_value(parameter_id: &str, value: u64) -> ModelOptionParameterValue {
        ModelOptionParameterValue {
            parameter_id: parameter_id.to_owned(),
            value: Some(Value::UnsignedInteger(value)),
        }
    }
}
