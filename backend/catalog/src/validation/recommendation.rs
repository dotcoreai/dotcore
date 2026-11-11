//! Validates the master catalog's model recommendations against the models and hardware classes
//! they name.
//!
//! Each hardware class's file lists, per use case, models and profiles that must be able to run
//! on that class and accept the requests the use case sends. The classes that declare
//! recommendations must be disjoint or strictly nested, so the list a target uses is never
//! ambiguous. Every problem is reported rather than only the first.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_proto::dotcore::catalog::v1::{
    HardwareClass, HardwareClassRecommendations, HardwareRequirements, HardwareSupport,
    InferenceRequestKind, Model, ModelEngineConfiguration, ModelRecommendation, UseCase,
};

use super::{CatalogDiagnostic, model::non_nested_classes};
use crate::selector::{SelectorRelation, selector_relation};

pub(super) fn validate_recommendations(
    recommendations: &HardwareClassRecommendations,
    models: &BTreeMap<String, Model>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
) -> Vec<anyhow::Error> {
    let mut errors = Vec::new();
    let class_id = &recommendations.hardware_class_id;
    let Some(hardware_class) = hardware_classes.get(class_id) else {
        errors.push(anyhow::anyhow!(
            "model recommendations reference unknown hardware class {class_id:?}"
        ));
        return errors;
    };
    let mut use_cases = BTreeSet::new();
    for list in &recommendations.use_cases {
        let use_case = match UseCase::try_from(list.use_case) {
            Ok(UseCase::Unspecified) | Err(_) => {
                errors.push(anyhow::anyhow!(
                    "model recommendations for hardware class {class_id:?} require a use case on every list"
                ));
                continue;
            }
            Ok(use_case) => use_case,
        };
        if !use_cases.insert(use_case) {
            errors.push(anyhow::anyhow!(
                "model recommendations for hardware class {class_id:?} list use case {} more than once",
                use_case.as_str_name()
            ));
        }
        let mut seen = BTreeSet::new();
        for (index, entry) in list.entries.iter().enumerate() {
            let label = format!(
                "model recommendations for hardware class {class_id:?} use case {} entry {} (model {:?} profile {:?})",
                use_case.as_str_name(),
                index + 1,
                entry.model_id,
                entry.profile_id
            );
            if !seen.insert((entry.model_id.as_str(), entry.profile_id.as_str())) {
                errors.push(anyhow::anyhow!("{label} repeats an earlier entry"));
                continue;
            }
            validate_entry(
                entry,
                hardware_class,
                models,
                hardware_classes,
                &label,
                &mut errors,
            );
        }
    }
    errors
}

/// Reports every recommendation file whose hardware class overlaps an earlier one's without one
/// nesting in the other, since a target matching both would have no single most specific list.
pub(super) fn report_overlapping_recommendation_classes(
    recommendations: &[(String, HardwareClassRecommendations)],
    hardware_classes: &BTreeMap<String, HardwareClass>,
    diagnostics: &mut Vec<CatalogDiagnostic>,
) {
    let mut earlier: Vec<&HardwareClass> = Vec::new();
    for (location, recommendations) in recommendations {
        let Some(hardware_class) = hardware_classes.get(&recommendations.hardware_class_id) else {
            continue;
        };
        for known in &earlier {
            if let Some((first, second)) = non_nested_classes(&[known, hardware_class])
                && first != second
            {
                diagnostics.push(CatalogDiagnostic::new(
                    location,
                    anyhow::anyhow!(
                        "model recommendations for hardware classes {first:?} and {second:?} overlap without one nesting in the other"
                    ),
                ));
            }
        }
        earlier.push(hardware_class);
    }
}

fn validate_entry(
    entry: &ModelRecommendation,
    hardware_class: &HardwareClass,
    models: &BTreeMap<String, Model>,
    hardware_classes: &BTreeMap<String, HardwareClass>,
    label: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    let Some(model) = models.get(&entry.model_id) else {
        errors.push(anyhow::anyhow!("{label} references an unknown model"));
        return;
    };
    let Some(configuration) = model
        .profiles
        .iter()
        .find(|profile| profile.profile_id == entry.profile_id)
        .and_then(|profile| {
            model
                .configurations
                .iter()
                .find(|configuration| configuration.configuration_id == profile.configuration_id)
        })
    else {
        errors.push(anyhow::anyhow!("{label} references an unknown profile"));
        return;
    };

    let related_support = related_hardware_support(configuration, hardware_class, hardware_classes);
    if related_support.is_empty() {
        errors.push(anyhow::anyhow!(
            "{label} runs a configuration that supports neither this hardware class, a class containing it, nor a class nested in it"
        ));
    }

    if !configuration
        .request_kinds
        .contains(&(InferenceRequestKind::Chat as i32))
    {
        errors.push(anyhow::anyhow!(
            "{label} runs a configuration that doesn't accept {} requests",
            InferenceRequestKind::Chat.as_str_name()
        ));
    }

    if let Some(requirements) = entry.hardware_requirements.as_ref()
        && !related_support.is_empty()
    {
        validate_entry_minimums(requirements, &related_support, label, errors);
    }
}

/// Returns the configuration's hardware support entries on classes equal to, containing, or
/// nested in the recommendation's class.
fn related_hardware_support<'a>(
    configuration: &'a ModelEngineConfiguration,
    hardware_class: &HardwareClass,
    hardware_classes: &BTreeMap<String, HardwareClass>,
) -> Vec<&'a HardwareSupport> {
    let Some(selector) = hardware_class.selector.as_ref() else {
        return Vec::new();
    };
    configuration
        .hardware_support
        .iter()
        .filter(|support| {
            hardware_classes
                .get(&support.hardware_class_id)
                .and_then(|class| class.selector.as_ref())
                .is_some_and(|supported| {
                    matches!(
                        selector_relation(supported, selector),
                        SelectorRelation::Equal
                            | SelectorRelation::Narrower
                            | SelectorRelation::Wider
                    )
                })
        })
        .collect()
}

/// Rejects an entry minimum below every related support's declared minimum: the configuration's
/// own minimum applies anyway, so the entry's would never separate machine sizes.
fn validate_entry_minimums(
    requirements: &HardwareRequirements,
    related_support: &[&HardwareSupport],
    label: &str,
    errors: &mut Vec<anyhow::Error>,
) {
    let lowest_declared = |field: fn(&HardwareRequirements) -> u64| {
        related_support
            .iter()
            .map(|support| support.hardware_requirements.as_ref().map_or(0, field))
            .min()
            .unwrap_or(0)
    };
    let mut check = |name: &str, entry_minimum: u64, configuration_minimum: u64| {
        if entry_minimum != 0 && entry_minimum < configuration_minimum {
            errors.push(anyhow::anyhow!(
                "{label} sets {name} {entry_minimum} bytes, below its configuration's {configuration_minimum} bytes"
            ));
        }
    };
    check(
        "minimum system memory",
        requirements.minimum_system_memory_bytes,
        lowest_declared(|requirements| requirements.minimum_system_memory_bytes),
    );
    check(
        "minimum memory per device",
        requirements.minimum_memory_bytes_per_device,
        lowest_declared(|requirements| requirements.minimum_memory_bytes_per_device),
    );
}
