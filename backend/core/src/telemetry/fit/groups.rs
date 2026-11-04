//! Splits a sizing fit's points into groups by the option choices the equation branches on, and
//! fits each group on its own.
//!
//! A group solves the parameters whose coefficients vary across its fit points, plus one free
//! intercept that absorbs every parameter whose coefficient is the same there. Comparing group
//! intercepts shows offsets that one shared intercept would spread across the slopes.

use std::collections::BTreeMap;

use serde::Serialize;

use super::{
    EvaluatedPoint, FitError, ResidualSummary,
    solve::{self, Row},
};

/// One group's own fit: the points sharing its option choices, solved with a free intercept.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupReport {
    /// Option choices every point of the group shares, keyed by option and parameter id.
    pub choices: BTreeMap<String, String>,

    /// Whether the choices are the configuration's defaults.
    pub default: bool,

    /// Fit points of the group.
    pub point_count: usize,

    /// Total context range of the fit points, when they carry one.
    pub context_range: Option<ContextRange>,

    /// Parameters whose coefficient varies across the fit points, rounded up.
    pub slopes: BTreeMap<String, u64>,

    /// Parameters whose coefficient is the same non-zero value on every fit point, with that
    /// coefficient; the intercept absorbs them.
    pub intercept_parameters: BTreeMap<String, u64>,

    /// Value left after the known terms and slopes, rounded up; none when the group fails.
    pub intercept: Option<i64>,

    /// This group's intercept minus the default group's; none for the default group or when
    /// either intercept is missing.
    pub intercept_delta: Option<i64>,

    /// Residuals of every evaluated point of the group against the group's own fit.
    pub residuals: Option<ResidualSummary>,

    /// Slope between each pair of adjacent fit contexts, from the observed values less the
    /// equation's known terms, independent of the fitted slopes.
    pub local_slopes: Vec<LocalSlope>,

    /// Why the group could not be fitted.
    pub error: Option<String>,
}

/// Smallest and largest total context of a set of points.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextRange {
    pub minimum: u64,
    pub maximum: u64,
}

/// Observed growth per context token between two adjacent fit contexts of one group.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LocalSlope {
    pub from_context: u64,
    pub to_context: u64,
    pub per_token: f64,
}

/// Fits every group of the evaluated points, each flagged as a fit point or not, and compares
/// their intercepts with the group matching the configuration's default choices.
pub(super) fn fit_groups(
    unknowns: &[String],
    points: &[(&EvaluatedPoint, bool)],
    default_choices: Option<&BTreeMap<String, String>>,
) -> Vec<GroupReport> {
    let mut grouped = BTreeMap::<&BTreeMap<String, String>, Vec<(&EvaluatedPoint, bool)>>::new();
    for (point, fitted) in points {
        grouped
            .entry(&point.choices)
            .or_default()
            .push((point, *fitted));
    }
    let mut groups = grouped
        .into_iter()
        .map(|(choices, points)| fit_group(unknowns, choices, &points, default_choices))
        .collect::<Vec<_>>();
    let default_intercept = groups
        .iter()
        .find(|group| group.default)
        .and_then(|group| group.intercept);
    for group in groups.iter_mut().filter(|group| !group.default) {
        group.intercept_delta = group
            .intercept
            .zip(default_intercept)
            .map(|(intercept, default)| intercept - default);
    }
    groups
}

/// Renders a group's choices for comments and messages, such as `kv-cache.precision=q8_0`.
pub(super) fn choices_label(choices: &BTreeMap<String, String>) -> String {
    if choices.is_empty() {
        return "every point".to_owned();
    }
    choices
        .iter()
        .map(|(choice, value)| format!("{choice}={value}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The total context range of the points, when any carries one.
pub(super) fn context_range<'a>(
    points: impl Iterator<Item = &'a EvaluatedPoint>,
) -> Option<ContextRange> {
    points
        .filter_map(|point| point.total_context)
        .fold(None, |range, context| {
            Some(match range {
                None => ContextRange {
                    minimum: context,
                    maximum: context,
                },
                Some(range) => ContextRange {
                    minimum: range.minimum.min(context),
                    maximum: range.maximum.max(context),
                },
            })
        })
}

fn fit_group(
    unknowns: &[String],
    choices: &BTreeMap<String, String>,
    points: &[(&EvaluatedPoint, bool)],
    default_choices: Option<&BTreeMap<String, String>>,
) -> GroupReport {
    let fit_points = points
        .iter()
        .filter(|(_, fitted)| *fitted)
        .map(|(point, _)| *point)
        .collect::<Vec<_>>();
    let mut report = GroupReport {
        choices: choices.clone(),
        default: default_choices == Some(choices),
        point_count: fit_points.len(),
        context_range: context_range(fit_points.iter().copied()),
        slopes: BTreeMap::new(),
        intercept_parameters: BTreeMap::new(),
        intercept: None,
        intercept_delta: None,
        residuals: None,
        local_slopes: local_slopes(&fit_points),
        error: None,
    };
    if fit_points.is_empty() {
        report.error = Some("the group has no fit point".to_owned());
        return report;
    }

    let mut slope_columns = Vec::new();
    for (column, name) in unknowns.iter().enumerate() {
        let first = fit_points[0].coefficients[column];
        if fit_points
            .iter()
            .any(|point| point.coefficients[column] != first)
        {
            slope_columns.push(column);
        } else if first != 0.0 {
            report
                .intercept_parameters
                .insert(name.clone(), first as u64);
        }
    }
    let slope_names = slope_columns
        .iter()
        .map(|&column| unknowns[column].clone())
        .chain(["intercept".to_owned()])
        .collect::<Vec<_>>();
    let rows = fit_points
        .iter()
        .map(|point| Row {
            target: point.target(),
            coefficients: slope_columns
                .iter()
                .map(|&column| point.coefficients[column])
                .chain([1.0])
                .collect(),
        })
        .collect::<Vec<_>>();
    let Some(values) = solve::least_squares(&rows) else {
        report.error = Some(match solve::diagnose(&slope_names, &rows) {
            Err(FitError::Unsolvable { cause, .. }) => cause.to_string(),
            _ => "the group's coefficients are linearly dependent".to_owned(),
        });
        return report;
    };

    let mut slopes = Vec::new();
    for (&column, value) in slope_columns.iter().zip(&values) {
        let rounded = solve::round_up(*value);
        if rounded < 0.0 {
            report.error = Some(format!(
                "slope {:?} is negative ({value})",
                unknowns[column]
            ));
            return report;
        }
        slopes.push((column, rounded));
    }
    let intercept_of = |point: &EvaluatedPoint| {
        point.target()
            - slopes
                .iter()
                .map(|(column, slope)| slope * point.coefficients[*column])
                .sum::<f64>()
    };
    let intercept = solve::round_up(
        fit_points
            .iter()
            .map(|point| intercept_of(point))
            .sum::<f64>()
            / fit_points.len() as f64,
    );
    let residuals = points
        .iter()
        .map(|(point, _)| (intercept_of(point) - intercept).round() as i64)
        .collect::<Vec<_>>();

    report.slopes = slopes
        .iter()
        .map(|(column, slope)| (unknowns[*column].clone(), *slope as u64))
        .collect();
    report.intercept = Some(intercept as i64);
    report.residuals = ResidualSummary::of(&residuals);
    report
}

/// Slopes between adjacent distinct contexts, averaging the points that share a context.
fn local_slopes(points: &[&EvaluatedPoint]) -> Vec<LocalSlope> {
    let mut by_context = BTreeMap::<u64, (f64, usize)>::new();
    for point in points {
        if let Some(context) = point.total_context {
            let entry = by_context.entry(context).or_insert((0.0, 0));
            entry.0 += point.target();
            entry.1 += 1;
        }
    }
    let averages = by_context
        .into_iter()
        .map(|(context, (sum, count))| (context, sum / count as f64))
        .collect::<Vec<_>>();
    averages
        .windows(2)
        .map(|pair| {
            let [(from_context, from), (to_context, to)] = [pair[0], pair[1]];
            LocalSlope {
                from_context,
                to_context,
                per_token: (to - from) / (to_context - from_context) as f64,
            }
        })
        .collect()
}
