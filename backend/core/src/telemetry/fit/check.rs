//! Checks a binding's stored sizing constants against workload resource records, fitting nothing.
//!
//! Each record's residual is measured against the estimate's equation, the measured footprint, and
//! its headroom against the whole estimate, equation plus allowance. Keeping them apart stops an
//! unused allowance budget, such as an empty prompt cache, from reading as slack in the model.

use std::collections::BTreeMap;

use dotcore_catalog::{Catalog, SizingFitEquation};
use dotcore_proto::dotcore::telemetry::v1::WorkloadResourceRecord;
use serde::Serialize;

use super::{
    CustomArgumentRecord, DefaultedOptions, FitEquation, FitError, FitRequest, ObservedQuantity,
    ResidualSummary, SeriesReport, SkippedRecord, defaulted_options, fact_values,
    groups::ContextRange, point_identity, record_inputs, select, signed_difference, total_context,
};

/// Stored constants compared with every matching record.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckReport {
    pub observed: String,
    pub points: Vec<CheckPointReport>,
    pub groups: Vec<CheckGroupReport>,

    /// Points the whole estimate under-predicts, largest shortfall first.
    pub under_predictions: Vec<CheckPointReport>,
    pub skipped: Vec<SkippedRecord>,
    pub series: SeriesReport,
    pub defaulted_options: Vec<DefaultedOptions>,
    pub custom_argument_records: Vec<CustomArgumentRecord>,
}

/// One record against the stored constants. A positive residual is an under-prediction of the
/// equation; a negative headroom is an under-prediction of the whole estimate.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckPointReport {
    pub record_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub node_id: String,
    pub accelerator: String,
    pub kind: &'static str,
    pub choices: BTreeMap<String, String>,
    pub total_context: Option<u64>,
    pub observed: u64,
    pub equation: u64,
    pub allowance: u64,
    pub residual: i64,
    pub headroom: i64,
}

/// Residuals and the smallest headroom of the records sharing one set of option choices.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CheckGroupReport {
    pub choices: BTreeMap<String, String>,
    pub point_count: usize,
    pub context_range: Option<ContextRange>,
    pub residuals: ResidualSummary,
    pub minimum_headroom: i64,
}

/// Evaluates the request's catalog estimate with its stored constants for every matching record.
/// The request must name a catalog estimate and no parameter, since nothing is fitted.
pub fn check(
    catalog: &Catalog,
    request: &FitRequest,
    records: &[WorkloadResourceRecord],
) -> Result<CheckReport, FitError> {
    let FitEquation::Estimate(resource) = &request.equation else {
        return Err(FitError::CheckRequiresStoredEstimate);
    };
    if !request.parameters.is_empty() {
        return Err(FitError::CheckRequiresStoredEstimate);
    }
    let selection = select(
        catalog,
        request,
        SizingFitEquation::Stored(resource),
        &[],
        records,
    )?;
    let model = &selection.target.model;
    let configuration = &selection.target.configuration;

    let mut points = Vec::new();
    let mut skipped = Vec::new();
    for record in selection.matching.iter().copied() {
        match check_point(model, configuration, &request.observed, record) {
            Ok(point) => points.push(point),
            Err(reason) => skipped.push(SkippedRecord {
                record_id: record.record_id.clone(),
                reason,
            }),
        }
    }

    let mut grouped = BTreeMap::<&BTreeMap<String, String>, Vec<&CheckPointReport>>::new();
    for point in &points {
        grouped.entry(&point.choices).or_default().push(point);
    }
    let groups = grouped
        .into_iter()
        .filter_map(|(choices, points)| {
            Some(CheckGroupReport {
                choices: choices.clone(),
                point_count: points.len(),
                context_range: context_range_of(&points),
                residuals: ResidualSummary::of(
                    &points
                        .iter()
                        .map(|point| point.residual)
                        .collect::<Vec<_>>(),
                )?,
                minimum_headroom: points.iter().map(|point| point.headroom).min()?,
            })
        })
        .collect();
    let mut under_predictions = points
        .iter()
        .filter(|point| point.headroom < 0)
        .cloned()
        .collect::<Vec<_>>();
    under_predictions.sort_by_key(|point| point.headroom);

    Ok(CheckReport {
        observed: request.observed.name(),
        defaulted_options: defaulted_options(configuration, selection.matching.iter().copied()),
        points,
        groups,
        under_predictions,
        skipped,
        series: selection.series,
        custom_argument_records: selection.custom_argument_records,
    })
}

fn check_point(
    model: &dotcore_catalog::SizingFitModel,
    configuration: &dotcore_proto::dotcore::catalog::v1::ModelEngineConfiguration,
    observed: &ObservedQuantity,
    record: &WorkloadResourceRecord,
) -> Result<CheckPointReport, String> {
    let describe = |error| format!("{:#}", anyhow::Error::new(error));
    let value = observed
        .value(record)
        .ok_or_else(|| format!("record has no {}", observed.name()))?;
    let (engine_values, option_values) = record_inputs(configuration, record);
    let facts = fact_values(record);
    if model
        .point(&engine_values, &option_values, &facts)
        .map_err(describe)?
        .is_none()
    {
        return Err("the estimate is unavailable for these inputs".to_owned());
    }
    let equation = model
        .predict(&BTreeMap::new(), &engine_values, &option_values, &facts)
        .map_err(describe)?;
    let allowance = model
        .allowance(&engine_values, &option_values, &facts)
        .map_err(describe)?;
    let identity = point_identity(record);
    Ok(CheckPointReport {
        record_id: record.record_id.clone(),
        owner_id: identity.owner_id,
        generation: identity.generation,
        node_id: identity.node_id,
        accelerator: identity.accelerator,
        kind: identity.kind,
        choices: model.group(&option_values).map_err(describe)?,
        total_context: total_context(record),
        observed: value,
        equation,
        allowance,
        residual: signed_difference(value, equation),
        headroom: signed_difference(equation.saturating_add(allowance), value),
    })
}

fn context_range_of(points: &[&CheckPointReport]) -> Option<ContextRange> {
    let contexts = points
        .iter()
        .filter_map(|point| point.total_context)
        .collect::<Vec<_>>();
    Some(ContextRange {
        minimum: *contexts.iter().min()?,
        maximum: *contexts.iter().max()?,
    })
}
