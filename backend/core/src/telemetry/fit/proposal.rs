//! Turns a sizing fit's group results into the constants a catalog binding stores.
//!
//! Each slope comes from the group that exercises it. A parameter the configuration's default
//! choices use, such as the base, comes from the default group's intercept; one only another
//! choice uses, such as a vision tower, comes from that group's intercept. The margin is the
//! largest under-prediction of the proposed equation over every evaluated point, warm peaks
//! included. The proposal renders
//! as a binding entry ready to paste, with a provenance comment naming the hardware class, engine,
//! ranges and residuals, but never the hosts.

use std::collections::{BTreeMap, BTreeSet};

use dotcore_catalog::SizingFitTarget;
use dotcore_proto::dotcore::telemetry::v1::WorkloadResourceRecord;
use serde::Serialize;

use super::{
    EvaluatedPoint, ResidualSummary,
    groups::{GroupReport, choices_label},
    predict, signed_difference,
};

/// Constants proposed for a binding, the margin, and the evidence behind them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Proposal {
    /// Proposed value of each fitted parameter the groups determine.
    pub constants: BTreeMap<String, ProposedConstant>,

    /// Fitted parameters the groups leave undetermined, with the reason.
    pub undetermined: BTreeMap<String, String>,

    /// Largest under-prediction of the proposed equation over every evaluated point, or zero;
    /// none while a parameter is undetermined.
    pub margin: Option<u64>,

    /// Binding constant of the estimate's allowance that holds the margin, when the allowance
    /// uses exactly one.
    pub margin_parameter: Option<String>,

    /// Residuals of every evaluated point against the proposed equation.
    pub residuals: Option<ResidualSummary>,

    /// The proposal as a binding sizing entry, with a provenance comment.
    pub block: String,
}

/// One proposed constant and the group result it comes from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ProposedConstant {
    pub value: u64,
    pub source: String,
}

/// Selection and evidence a proposal is built from.
pub(super) struct ProposalInputs<'a> {
    pub(super) target: &'a SizingFitTarget,
    pub(super) hardware_class_id: &'a str,
    pub(super) binding_id: &'a str,
    pub(super) groups: &'a [GroupReport],

    /// Fitted parameters with a non-zero coefficient under the configuration's default choices.
    pub(super) default_parameters: &'a BTreeSet<String>,
    pub(super) evaluated: &'a [(&'a WorkloadResourceRecord, EvaluatedPoint)],
}

/// Proposes every fitted parameter's value from the groups, then the margin when all are known.
/// A value read from an intercept may need others first, such as the base an additive vision term
/// shares its group's intercept with, so proposing repeats until a round settles nothing new.
pub(super) fn propose(inputs: &ProposalInputs<'_>) -> Proposal {
    let unknowns = inputs.target.model.unknowns();
    let mut constants = BTreeMap::<String, ProposedConstant>::new();
    let mut undetermined = BTreeMap::new();
    loop {
        undetermined.clear();
        let mut settled = false;
        let pending = unknowns
            .iter()
            .filter(|name| !constants.contains_key(*name))
            .collect::<Vec<_>>();
        for unknown in pending {
            let resolved = constants
                .iter()
                .map(|(name, constant)| (name.clone(), constant.value))
                .collect();
            match propose_constant(
                unknown,
                inputs.groups,
                inputs.default_parameters.contains(unknown),
                &resolved,
            ) {
                Ok(constant) => {
                    constants.insert(unknown.clone(), constant);
                    settled = true;
                }
                Err(reason) => {
                    undetermined.insert(unknown.clone(), reason);
                }
            }
        }
        if !settled || undetermined.is_empty() {
            break;
        }
    }

    let residuals = if undetermined.is_empty() {
        let values = constants
            .iter()
            .map(|(name, constant)| (name.clone(), constant.value))
            .collect::<BTreeMap<_, _>>();
        inputs
            .evaluated
            .iter()
            .filter_map(|(record, point)| {
                let predicted = predict(
                    &inputs.target.model,
                    &inputs.target.configuration,
                    &values,
                    record,
                )
                .ok()?;
                Some(signed_difference(point.observed, predicted))
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let summary = ResidualSummary::of(&residuals);
    let margin = undetermined.is_empty().then(|| {
        summary
            .as_ref()
            .map_or(0, |summary| summary.maximum.max(0).unsigned_abs())
    });
    let margin_parameter = match inputs.target.model.allowance_parameters() {
        [parameter] => Some(parameter.clone()),
        _ => None,
    };
    let block = block(
        inputs,
        &constants,
        &undetermined,
        margin,
        margin_parameter.as_ref(),
        summary.as_ref(),
    );
    Proposal {
        constants,
        undetermined,
        margin,
        margin_parameter,
        residuals: summary,
        block,
    }
}

/// A parameter's value: its slope in the default group, else its largest slope in any group, else
/// what remains of a group's intercept once the other parameters it absorbs are resolved. That
/// group is the default one when the default choices use the parameter, so another choice's offset
/// never reads as the base.
fn propose_constant(
    unknown: &str,
    groups: &[GroupReport],
    default_parameter: bool,
    resolved: &BTreeMap<String, u64>,
) -> Result<ProposedConstant, String> {
    let solved = groups
        .iter()
        .filter(|group| group.error.is_none())
        .collect::<Vec<_>>();
    if let Some(group) = solved
        .iter()
        .find(|group| group.default && group.slopes.contains_key(unknown))
    {
        return Ok(ProposedConstant {
            value: group.slopes[unknown],
            source: format!(
                "slope of the default group {}",
                choices_label(&group.choices)
            ),
        });
    }
    let sloped = solved
        .iter()
        .filter_map(|group| Some((group.slopes.get(unknown)?, *group)))
        .collect::<Vec<_>>();
    if let Some((value, group)) = sloped.iter().max_by_key(|(value, _)| **value) {
        return Ok(ProposedConstant {
            value: **value,
            source: if sloped.len() == 1 {
                format!("slope of the group {}", choices_label(&group.choices))
            } else {
                format!(
                    "largest slope of {} groups, from {}",
                    sloped.len(),
                    choices_label(&group.choices)
                )
            },
        });
    }

    let candidates = if default_parameter {
        let Some(default) = groups.iter().find(|group| group.default) else {
            return Err("no record has the configuration's default option choices".to_owned());
        };
        vec![default]
    } else {
        groups
            .iter()
            .filter(|group| group.intercept_parameters.contains_key(unknown))
            .collect()
    };
    let mut values = Vec::new();
    let mut reasons = Vec::new();
    for group in candidates {
        match intercept_value(unknown, group, resolved) {
            Ok(value) => values.push((value, group)),
            Err(reason) => reasons.push(reason),
        }
    }
    match values.iter().max_by_key(|(value, _)| *value) {
        Some((value, group)) => Ok(ProposedConstant {
            value: *value,
            source: format!(
                "intercept of the {}group {}",
                if group.default { "default " } else { "" },
                choices_label(&group.choices)
            ),
        }),
        None if reasons.is_empty() => Err("no group exercises it".to_owned()),
        None => Err(reasons.join("; ")),
    }
}

/// The parameter's share of the group's intercept, once every other parameter the intercept
/// absorbs is resolved.
fn intercept_value(
    unknown: &str,
    group: &GroupReport,
    resolved: &BTreeMap<String, u64>,
) -> Result<u64, String> {
    let label = choices_label(&group.choices);
    let Some(&coefficient) = group.intercept_parameters.get(unknown) else {
        return Err(format!("the group {label} does not exercise it"));
    };
    if let Some(error) = &group.error {
        return Err(format!("the group {label} failed: {error}"));
    }
    let unresolved = group
        .intercept_parameters
        .keys()
        .filter(|name| *name != unknown && !resolved.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        return Err(format!(
            "the intercept of the group {label} also absorbs {}",
            unresolved.join(", ")
        ));
    }
    let others = group
        .intercept_parameters
        .iter()
        .filter(|(name, _)| *name != unknown)
        .map(|(name, coefficient)| i128::from(resolved[name]) * i128::from(*coefficient))
        .sum::<i128>();
    let share = i128::from(group.intercept.unwrap_or_default()) - others;
    if share < 0 {
        return Err(format!(
            "the intercept of the group {label} leaves it negative ({share})"
        ));
    }
    let coefficient = i128::from(coefficient.max(1));
    Ok(((share + coefficient - 1) / coefficient) as u64)
}

/// Renders the binding sizing entry: a provenance comment, then every binding constant in name
/// order, proposed or margin values first, stored values marked as such, so pasting it loses none.
fn block(
    inputs: &ProposalInputs<'_>,
    constants: &BTreeMap<String, ProposedConstant>,
    undetermined: &BTreeMap<String, String>,
    margin: Option<u64>,
    margin_parameter: Option<&String>,
    residuals: Option<&ResidualSummary>,
) -> String {
    let records = inputs.evaluated.iter().map(|(record, _)| *record);
    let accelerators = records
        .clone()
        .filter_map(|record| Some(record.node.as_ref()?.devices.first()?.model.clone()))
        .collect::<BTreeSet<_>>();
    let engines = records
        .filter_map(|record| {
            let inputs = record.inputs.as_ref()?;
            Some(format!(
                "{}/{}",
                inputs.engine_release_id, inputs.engine_artifact_id
            ))
        })
        .collect::<BTreeSet<_>>();
    let mut lines = vec![format!(
        "# Fitted on hardware class {:?} ({}) with {}, from {} records.",
        inputs.hardware_class_id,
        join(&accelerators),
        join(&engines),
        inputs.evaluated.len()
    )];
    for group in inputs.groups {
        let range = group.context_range.map_or_else(String::new, |range| {
            format!(" at total context {}-{}", range.minimum, range.maximum)
        });
        let delta = group.intercept_delta.map_or_else(String::new, |delta| {
            format!(", intercept {delta:+} from the default")
        });
        lines.push(format!(
            "# {}: {} fit points{range}{delta}.",
            choices_label(&group.choices),
            group.point_count
        ));
    }
    if let Some(residuals) = residuals {
        lines.push(format!(
            "# Residuals against these constants: {} to {}.",
            residuals.minimum, residuals.maximum
        ));
    }
    for (name, reason) in undetermined {
        lines.push(format!("# {name} is undetermined: {reason}."));
    }
    if let (Some(margin), None) = (margin, margin_parameter) {
        lines.push(format!(
            "# The margin of {margin} has no single allowance constant to hold it."
        ));
    }
    lines.push(format!("- bindingId: {:?}", inputs.binding_id));
    lines.push("  parameters:".to_owned());
    let stored = inputs.target.model.binding_constants();
    let names = stored
        .keys()
        .chain(constants.keys())
        .chain(margin_parameter)
        .collect::<BTreeSet<_>>();
    for name in names {
        let value = if let Some(constant) = constants.get(name) {
            constant.value.to_string()
        } else if let (Some(margin), Some(parameter)) = (margin, margin_parameter)
            && parameter == name
        {
            margin.to_string()
        } else if undetermined.contains_key(name) {
            format!("{}  # stored, undetermined", stored[name])
        } else {
            format!("{}  # stored, not fitted", stored[name])
        };
        lines.push(format!("    {name}: {value}"));
    }
    lines.join("\n") + "\n"
}

fn join(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(", ")
}
