//! Fits a catalog sizing equation's constants from workload resource records, and checks stored
//! constants against them, for development sizing work.
//!
//! Only records of the requested model configuration, hardware class, artifact binding, and
//! approved engine artifacts count; a record that names no hardware class never does, since its
//! sizing is unknown. Each realization contributes one fit point (its startup record, else its
//! first measurement); later peaks count only toward the margin and the observational warm-up
//! growth. A fit solves the estimate's measured equation, never its allowance: jointly over every
//! point, and per group of points sharing the option choices the equation branches on. It then
//! proposes the constants a binding stores. An unsolvable fit names its cause and a parameter set
//! the same points would solve.
//!
//! A binding whose configuration options realize different auxiliary artifacts has several
//! measurement series, and a fit reads the ones its selection names. The report states which
//! series it read, which realizable series it left out, and how many records each holds. A record
//! launched before its configuration declared an option is evaluated at the configuration's
//! default for that option, and the report names it.

use std::{
    collections::{BTreeMap, BTreeSet},
    str::FromStr,
};

use dotcore_catalog::{
    Catalog, SizingFitEquation, SizingFitModel, SizingFitTarget,
    shared_parameters::TOTAL_CONTEXT_PARAMETER_ID,
};
use dotcore_proto::dotcore::{
    catalog::v1::{ModelEngineConfiguration, ModelOptionValues, ParameterValues, parameter_value},
    telemetry::v1::{
        VllmStartupFacts, WorkloadResourceObservationKind, WorkloadResourceRecord,
        workload_resource_observation::EngineFacts,
    },
};
use serde::Serialize;

use crate::model::engine::selectable_auxiliary_artifact_sets;

mod check;
mod groups;
mod proposal;
mod solve;

pub use check::{CheckGroupReport, CheckPointReport, CheckReport, check};
pub use groups::{ContextRange, GroupReport, LocalSlope};
pub use proposal::{Proposal, ProposedConstant};
pub use solve::UnsolvableCause;

/// Selection, equation, and observed quantity of one sizing fit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FitRequest {
    pub model_id: String,
    pub configuration_id: String,
    /// Supported hardware class of the configuration whose sizing is fitted.
    pub hardware_class_id: String,
    pub binding_id: String,
    pub equation: FitEquation,
    /// Parameters to fit; empty fits the binding constants a catalog estimate uses.
    pub parameters: Vec<String>,
    pub observed: ObservedQuantity,
    /// Measurement series to read; the binding's declared series by default.
    pub series: SeriesSelection,
    /// Whether records launched with custom arguments are fitted; they are skipped by default
    /// because a custom argument may change memory the equation does not model.
    pub include_custom_argument_records: bool,
}

/// Equation whose constants are fitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FitEquation {
    /// The binding's declared catalog estimate for this resource, such as `acceleratorMemory`.
    Estimate(String),

    /// An ad hoc equation that may also use `${vllm.<fact>}` symbols.
    Inline(String),
}

/// Record value the equation must predict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservedQuantity {
    /// Accelerator memory attributed to the workload.
    AttributedBytes,

    /// One vLLM startup fact.
    VllmFact(VllmFact),

    /// One unsigned engine parameter value of the launch, such as `kv-cache-memory-bytes`.
    EngineParameter(String),
}

impl FromStr for ObservedQuantity {
    type Err = FitError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == "attributed-bytes" {
            return Ok(Self::AttributedBytes);
        }
        if let Some(parameter) = value.strip_prefix("engine.") {
            return Ok(Self::EngineParameter(parameter.to_owned()));
        }
        VllmFact::ALL
            .into_iter()
            .find(|fact| fact.symbol() == value)
            .map(Self::VllmFact)
            .ok_or_else(|| FitError::UnknownQuantity(value.to_owned()))
    }
}

impl ObservedQuantity {
    fn name(&self) -> String {
        match self {
            Self::AttributedBytes => "attributed-bytes".to_owned(),
            Self::VllmFact(fact) => fact.symbol().to_owned(),
            Self::EngineParameter(parameter) => format!("engine.{parameter}"),
        }
    }

    fn value(&self, record: &WorkloadResourceRecord) -> Option<u64> {
        let observation = record.observation.as_ref()?;
        match self {
            Self::AttributedBytes => Some(observation.attributed_memory_bytes),
            Self::VllmFact(fact) => match &observation.engine_facts {
                Some(EngineFacts::Vllm(facts)) => fact.value(facts),
                None => None,
            },
            Self::EngineParameter(parameter) => record
                .inputs
                .as_ref()?
                .engine_parameter_values
                .as_ref()?
                .values
                .iter()
                .find(|value| &value.parameter_id == parameter)
                .and_then(|value| match value.value {
                    Some(parameter_value::Value::UnsignedInteger(value)) => Some(value),
                    _ => None,
                }),
        }
    }
}

/// Which measurement series of a binding a fit reads.
///
/// A series is the set of auxiliary artifacts a launch's options add to the binding's declared ones,
/// so a configuration with two boolean option realizations has up to four series.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SeriesSelection {
    /// The one series whose realized auxiliary artifacts equal the binding's declared set.
    Declared,

    /// The named series, each named by the option-selected auxiliary artifacts it adds.
    Named(BTreeSet<BTreeSet<String>>),

    /// Every series the binding's configuration can realize.
    AllRealizable,
}

impl SeriesSelection {
    /// Parses the repeated series arguments; no argument keeps the binding's declared series.
    pub fn from_values(values: &[String]) -> Result<Self, FitError> {
        if values.is_empty() {
            return Ok(Self::Declared);
        }
        let mut named = BTreeSet::new();
        let mut all = false;
        for value in values {
            match parse_series(value)? {
                SeriesArgument::All => all = true,
                SeriesArgument::Named(series) => {
                    named.insert(series);
                }
            }
        }
        match (all, named.is_empty()) {
            (true, true) => Ok(Self::AllRealizable),
            (true, false) => Err(FitError::AllWithNamedSeries),
            (false, _) => Ok(Self::Named(named)),
        }
    }

    fn mode(&self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Named(_) => "named",
            Self::AllRealizable => "all",
        }
    }
}

/// vLLM startup facts usable as observed quantities and as `${vllm.<fact>}` equation symbols.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VllmFact {
    KvTokenCapacity,

    KvCacheReservedBytes,

    ModelLoadingBytes,

    CudaGraphBytes,

    StartupCudaFreeBytes,
}

impl VllmFact {
    pub const ALL: [Self; 5] = [
        Self::KvTokenCapacity,
        Self::KvCacheReservedBytes,
        Self::ModelLoadingBytes,
        Self::CudaGraphBytes,
        Self::StartupCudaFreeBytes,
    ];

    pub fn symbol(self) -> &'static str {
        match self {
            Self::KvTokenCapacity => "vllm.kv-token-capacity",
            Self::KvCacheReservedBytes => "vllm.kv-cache-reserved-bytes",
            Self::ModelLoadingBytes => "vllm.model-loading-bytes",
            Self::CudaGraphBytes => "vllm.cuda-graph-bytes",
            Self::StartupCudaFreeBytes => "vllm.startup-cuda-free-bytes",
        }
    }

    fn value(self, facts: &VllmStartupFacts) -> Option<u64> {
        match self {
            Self::KvTokenCapacity => facts.kv_cache_token_capacity,
            Self::KvCacheReservedBytes => facts.kv_cache_reserved_bytes,
            Self::ModelLoadingBytes => facts.model_loading_bytes,
            Self::CudaGraphBytes => facts.cuda_graph_bytes,
            Self::StartupCudaFreeBytes => facts.startup_cuda_free_bytes,
        }
    }
}

/// Fitted constants, per-group fits, the proposed binding constants, and the evidence behind them.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FitReport {
    pub observed: String,
    pub point_count: usize,

    /// Parameters fitted by default that no fit point exercises, left at their stored values.
    pub unfitted_parameters: Vec<String>,

    /// One solution of every fitted parameter shared by all points.
    pub joint: JointFit,

    /// Fits of each group of points sharing the option choices the equation branches on.
    pub groups: Vec<GroupReport>,

    /// Constants proposed for the binding from the groups, with the margin.
    pub proposal: Proposal,
    pub warm_growth: WarmGrowthReport,
    pub skipped: Vec<SkippedRecord>,

    /// Measurement series the fit read, and the realizable ones it left out.
    pub series: SeriesReport,

    /// Evaluated records that lack values for options of the configuration, which the fit took at
    /// the configuration's defaults.
    pub defaulted_options: Vec<DefaultedOptions>,

    /// Matching records launched with custom arguments that the fit left out; empty when the
    /// request includes them.
    pub custom_argument_records: Vec<CustomArgumentRecord>,
}

/// The fitted parameters solved over every fit point at once, with one shared intercept, and
/// every evaluated record compared with that solution.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JointFit {
    pub parameters: BTreeMap<String, u64>,
    pub points: Vec<FitPointReport>,
    pub residuals_by_accelerator: BTreeMap<String, ResidualSummary>,
    pub largest_under_prediction: Option<FitPointReport>,
}

/// One record compared with the fitted equation; a positive residual is an under-prediction.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct FitPointReport {
    pub record_id: String,
    pub owner_id: String,
    pub generation: u64,
    pub node_id: String,
    pub accelerator: String,
    pub kind: &'static str,

    /// Option choices of the record's group.
    pub choices: BTreeMap<String, String>,

    /// Whether the record was a fit point rather than a margin-only observation.
    pub fitted: bool,
    pub observed: u64,
    pub predicted: u64,
    pub residual: i64,
}

/// A matching record that could not be used, with the reason.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SkippedRecord {
    pub record_id: String,
    pub reason: String,
}

/// A matching record launched with custom arguments, with the arguments as recorded.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustomArgumentRecord {
    pub record_id: String,
    pub custom_argument_tokens: Vec<String>,
}

/// A record evaluated at the configuration's defaults for the options it carries no value for,
/// such as one launched before the configuration declared them.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DefaultedOptions {
    pub record_id: String,
    pub option_ids: Vec<String>,
}

/// Residual range of one accelerator model's points.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ResidualSummary {
    pub count: usize,
    pub minimum: i64,
    pub maximum: i64,
    pub mean: f64,
}

impl ResidualSummary {
    /// Summarizes the residuals, or none when there are none.
    pub(super) fn of(residuals: &[i64]) -> Option<Self> {
        Some(Self {
            count: residuals.len(),
            minimum: *residuals.iter().min()?,
            maximum: *residuals.iter().max()?,
            mean: residuals.iter().map(|value| *value as f64).sum::<f64>() / residuals.len() as f64,
        })
    }
}

/// Attributed memory growth after the fit point, driven by request load the records do not describe.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WarmGrowthReport {
    pub observational: bool,
    pub maximum_growth_bytes: u64,
    pub realizations: Vec<RealizationGrowth>,
}

/// Growth of one realization from its fit point to its largest observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RealizationGrowth {
    pub owner_id: String,
    pub generation: u64,
    pub fit_point_bytes: u64,
    pub maximum_bytes: u64,
    pub growth_bytes: u64,
}
/// Which measurement series a fit read, and what its selection left behind.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SeriesReport {
    /// How the series were selected: `declared`, `named`, or `all`.
    pub mode: &'static str,

    /// Series the fit read, in sorted order.
    pub read: Vec<SeriesRecords>,

    /// Realizable series the selection left out, in sorted order.
    pub ignored: Vec<SeriesRecords>,

    /// Records matching the binding whose realized artifacts are no series of it.
    pub unmatched_record_count: usize,
}

/// One series of a binding and the records it holds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SeriesRecords {
    /// Option-selected auxiliary artifacts of the series, without the binding's declared ones.
    pub auxiliary_artifact_ids: Vec<String>,

    pub record_count: usize,
}

/// Failure to fit a sizing equation.
#[derive(Debug, thiserror::Error)]
pub enum FitError {
    #[error(
        "unknown observed quantity {0:?}; use attributed-bytes, vllm.<fact>, or engine.<parameter>"
    )]
    UnknownQuantity(String),

    #[error("sizing fit is invalid")]
    Sizing(#[source] anyhow::Error),

    #[error("engine kind of model configuration {configuration_id:?} is not supported")]
    UnsupportedEngineKind { configuration_id: String },

    #[error("sizing fit is unsolvable: {cause}; {}", solvable_hint(.solvable))]
    Unsolvable {
        cause: UnsolvableCause,
        /// Parameters, in order, that the same fit points determine.
        solvable: Vec<String>,
    },

    #[error("fitted parameter {parameter:?} is negative ({value})")]
    NegativeParameter { parameter: String, value: f64 },

    #[error("series {0:?} must be `all`, `none`, or comma-separated auxiliary artifact ids")]
    InvalidSeries(String),

    #[error("series `all` cannot be combined with named series")]
    AllWithNamedSeries,

    #[error(
        "series {series:?} names {unknown}, which configuration {configuration_id:?} cannot realize"
    )]
    UnrealizableSeries {
        configuration_id: String,
        series: String,
        unknown: String,
    },

    #[error(
        "no telemetry record matches {selection}; refusing to fit a different series ({custom_argument_record_count} matching records with custom arguments were skipped)"
    )]
    NoMatchingRecords {
        selection: String,
        custom_argument_record_count: usize,
    },

    #[error(
        "a sizing check evaluates a catalog estimate's stored constants, so it takes no inline equation or fitted parameter"
    )]
    CheckRequiresStoredEstimate,
}

/// Fits the request's parameters from the records: one joint solution, a fit per group of option
/// choices, and the constants proposed for the binding, with residuals, margin, and growth.
pub fn fit(
    catalog: &Catalog,
    request: &FitRequest,
    records: &[WorkloadResourceRecord],
) -> Result<FitReport, FitError> {
    let equation = match &request.equation {
        FitEquation::Estimate(resource) => SizingFitEquation::Estimate(resource),
        FitEquation::Inline(equation) => SizingFitEquation::Inline(equation),
    };
    let mut selection = select(catalog, request, equation, &request.parameters, records)?;
    let (mut evaluated, mut skipped) = evaluate_records(&selection, &request.observed);
    let fit_records = fit_record_ids(evaluated.iter().map(|(record, _)| *record));

    // A default parameter no fit point exercises, such as the slope of an option choice no record
    // made, keeps its stored value instead of making the fit unsolvable.
    let mut unfitted_parameters = Vec::new();
    if request.parameters.is_empty() {
        let unknowns = selection.target.model.unknowns().to_vec();
        let (without_data, with_data): (Vec<_>, Vec<_>) =
            unknowns.iter().enumerate().partition(|(column, _)| {
                fit_points(&evaluated, &fit_records).all(|point| point.coefficients[*column] == 0.0)
            });
        if !without_data.is_empty() && !with_data.is_empty() {
            let fitted = with_data
                .into_iter()
                .map(|(_, name)| name.clone())
                .collect::<Vec<_>>();
            selection.target = fit_target(catalog, request, equation, &fitted)?;
            (evaluated, skipped) = evaluate_records(&selection, &request.observed);
            unfitted_parameters = without_data
                .into_iter()
                .map(|(_, name)| name.clone())
                .collect();
        }
    }

    let model = &selection.target.model;
    let configuration = &selection.target.configuration;
    let rows = fit_points(&evaluated, &fit_records)
        .map(|point| solve::Row {
            target: point.target(),
            coefficients: point.coefficients.clone(),
        })
        .collect::<Vec<_>>();
    let parameters = model
        .unknowns()
        .iter()
        .cloned()
        .zip(solve::solve(model.unknowns(), &rows)?)
        .collect::<BTreeMap<_, _>>();

    let mut points = Vec::new();
    for (record, point) in &evaluated {
        match predict(model, configuration, &parameters, record) {
            Ok(predicted) => points.push(point_report(
                record,
                &point.choices,
                fit_records.contains(&record.record_id),
                point.observed,
                predicted,
            )),
            Err(reason) => skipped.push(SkippedRecord {
                record_id: record.record_id.clone(),
                reason,
            }),
        }
    }
    let largest_under_prediction = points
        .iter()
        .filter(|point| point.residual > 0)
        .max_by_key(|point| point.residual)
        .cloned();

    let default_choices = model.group(&default_option_values(configuration)).ok();
    let groups = groups::fit_groups(
        model.unknowns(),
        &evaluated
            .iter()
            .map(|(record, point)| (point, fit_records.contains(&record.record_id)))
            .collect::<Vec<_>>(),
        default_choices.as_ref(),
    );
    let proposal = proposal::propose(&proposal::ProposalInputs {
        target: &selection.target,
        hardware_class_id: &request.hardware_class_id,
        binding_id: &request.binding_id,
        groups: &groups,
        default_parameters: &default_parameters(model, configuration, &evaluated),
        evaluated: &evaluated,
    });
    Ok(FitReport {
        observed: request.observed.name(),
        point_count: rows.len(),
        unfitted_parameters,
        joint: JointFit {
            parameters,
            residuals_by_accelerator: residual_summaries(&points),
            points,
            largest_under_prediction,
        },
        groups,
        proposal,
        // Growth uses every matching record: peaks lack engine facts and may not evaluate.
        warm_growth: warm_growth(selection.matching.iter().copied(), &fit_records),
        skipped,
        series: selection.series,
        defaulted_options: defaulted_options(
            configuration,
            evaluated.iter().map(|(record, _)| *record),
        ),
        custom_argument_records: selection.custom_argument_records,
    })
}

/// The catalog sizing a request names and the records it reads.
struct Selection<'a> {
    target: SizingFitTarget,
    matching: Vec<&'a WorkloadResourceRecord>,
    series: SeriesReport,
    custom_argument_records: Vec<CustomArgumentRecord>,
}

/// Resolves the request's catalog sizing, then keeps the records of its model configuration,
/// hardware class, binding, approved engine artifacts and selected series.
fn select<'a>(
    catalog: &Catalog,
    request: &FitRequest,
    equation: SizingFitEquation<'_>,
    unknowns: &[String],
    records: &'a [WorkloadResourceRecord],
) -> Result<Selection<'a>, FitError> {
    let target = fit_target(catalog, request, equation, unknowns)?;
    let binding_auxiliaries = target
        .binding
        .auxiliary_artifact_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let realizable = selectable_auxiliary_artifact_sets(target.engine_kind, &target.configuration)
        .ok_or_else(|| FitError::UnsupportedEngineKind {
            configuration_id: request.configuration_id.clone(),
        })?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let accepted = accepted_series(&request.series, &realizable, &request.configuration_id)?;
    let mut resolved = accepted
        .iter()
        .cloned()
        .map(|series| (series, 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut ignored = BTreeMap::<BTreeSet<String>, usize>::new();
    let mut unmatched_record_count = 0;
    let mut matching = Vec::new();
    let mut custom_argument_records = Vec::new();
    for record in records {
        let Some(inputs) = record.inputs.as_ref() else {
            continue;
        };
        if inputs.model_id != request.model_id
            || inputs.configuration_id != request.configuration_id
            || inputs.hardware_class_id != request.hardware_class_id
            || inputs.model_artifact_id != target.binding.model_artifact_id
            || !target
                .configuration
                .engine_artifacts
                .iter()
                .any(|artifact| {
                    artifact.engine_release_id == inputs.engine_release_id
                        && artifact.engine_artifact_id == inputs.engine_artifact_id
                })
        {
            continue;
        }
        if !request.include_custom_argument_records && !inputs.custom_argument_tokens.is_empty() {
            custom_argument_records.push(CustomArgumentRecord {
                record_id: record.record_id.clone(),
                custom_argument_tokens: inputs.custom_argument_tokens.clone(),
            });
            continue;
        }
        let realized = inputs
            .auxiliary_artifact_ids
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let Some(added) = added_artifacts(&realized, &binding_auxiliaries) else {
            unmatched_record_count += 1;
            continue;
        };
        if accepted.contains(&added) {
            *resolved.entry(added).or_insert(0) += 1;
            matching.push(record);
        } else if realizable.contains(&added) {
            *ignored.entry(added).or_insert(0) += 1;
        } else {
            unmatched_record_count += 1;
        }
    }
    if matching.is_empty() {
        return Err(FitError::NoMatchingRecords {
            selection: accepted_series_label(&accepted),
            custom_argument_record_count: custom_argument_records.len(),
        });
    }
    Ok(Selection {
        target,
        matching,
        series: SeriesReport {
            mode: request.series.mode(),
            read: series_records(resolved),
            ignored: series_records(ignored),
            unmatched_record_count,
        },
        custom_argument_records,
    })
}

fn fit_target(
    catalog: &Catalog,
    request: &FitRequest,
    equation: SizingFitEquation<'_>,
    unknowns: &[String],
) -> Result<SizingFitTarget, FitError> {
    let extra_symbols = VllmFact::ALL
        .into_iter()
        .map(|fact| fact.symbol().to_owned())
        .collect::<Vec<_>>();
    catalog
        .sizing_fit(
            &request.model_id,
            &request.configuration_id,
            &request.hardware_class_id,
            &request.binding_id,
            equation,
            unknowns,
            &extra_symbols,
        )
        .map_err(|error| FitError::Sizing(anyhow::Error::new(error)))
}

/// Evaluates every selected record, keeping the reason for each one that does not evaluate.
fn evaluate_records<'a>(
    selection: &Selection<'a>,
    observed: &ObservedQuantity,
) -> (
    Vec<(&'a WorkloadResourceRecord, EvaluatedPoint)>,
    Vec<SkippedRecord>,
) {
    let mut evaluated = Vec::new();
    let mut skipped = Vec::new();
    for record in selection.matching.iter().copied() {
        match evaluate(
            &selection.target.model,
            &selection.target.configuration,
            observed,
            record,
        ) {
            Ok(point) => evaluated.push((record, point)),
            Err(reason) => skipped.push(SkippedRecord {
                record_id: record.record_id.clone(),
                reason,
            }),
        }
    }
    (evaluated, skipped)
}

fn fit_points<'a>(
    evaluated: &'a [(&WorkloadResourceRecord, EvaluatedPoint)],
    fit_records: &'a BTreeSet<String>,
) -> impl Iterator<Item = &'a EvaluatedPoint> {
    evaluated
        .iter()
        .filter(|(record, _)| fit_records.contains(&record.record_id))
        .map(|(_, point)| point)
}

/// Every option's configuration default, the choices a deployment makes unless it overrides them.
fn default_option_values(configuration: &ModelEngineConfiguration) -> Vec<ModelOptionValues> {
    configuration
        .options
        .iter()
        .filter_map(|option| option.default_values.clone())
        .collect()
}

/// Fitted parameters the configuration's default choices use: those with a non-zero coefficient
/// when the first evaluated record's inputs are taken at the default option values.
fn default_parameters(
    model: &SizingFitModel,
    configuration: &ModelEngineConfiguration,
    evaluated: &[(&WorkloadResourceRecord, EvaluatedPoint)],
) -> BTreeSet<String> {
    let Some((record, _)) = evaluated.first() else {
        return BTreeSet::new();
    };
    let (engine_values, _) = record_inputs(configuration, record);
    let Ok(Some(point)) = model.point(
        &engine_values,
        &default_option_values(configuration),
        &fact_values(record),
    ) else {
        return BTreeSet::new();
    };
    model
        .unknowns()
        .iter()
        .zip(point.coefficients)
        .filter(|(_, coefficient)| *coefficient != 0)
        .map(|(name, _)| name.clone())
        .collect()
}

/// The records evaluated at the configuration's default for options they carry no value for.
fn defaulted_options<'a>(
    configuration: &ModelEngineConfiguration,
    records: impl Iterator<Item = &'a WorkloadResourceRecord>,
) -> Vec<DefaultedOptions> {
    records
        .filter_map(|record| {
            let option_ids = defaulted_option_ids(configuration, record);
            (!option_ids.is_empty()).then(|| DefaultedOptions {
                record_id: record.record_id.clone(),
                option_ids,
            })
        })
        .collect()
}

/// Series the selection reads, refusing a named series the configuration cannot realize.
fn accepted_series(
    selection: &SeriesSelection,
    realizable: &BTreeSet<BTreeSet<String>>,
    configuration_id: &str,
) -> Result<BTreeSet<BTreeSet<String>>, FitError> {
    match selection {
        SeriesSelection::Declared => Ok(BTreeSet::from([BTreeSet::new()])),
        SeriesSelection::AllRealizable => Ok(realizable.clone()),
        SeriesSelection::Named(named) => named
            .iter()
            .map(|series| {
                if realizable.contains(series) {
                    return Ok(series.clone());
                }
                let realizable_artifacts = realizable.iter().flatten().collect::<BTreeSet<_>>();
                let unknown = series
                    .iter()
                    .filter(|artifact| !realizable_artifacts.contains(artifact))
                    .cloned()
                    .collect::<Vec<_>>();
                Err(FitError::UnrealizableSeries {
                    configuration_id: configuration_id.to_owned(),
                    series: series_label(series),
                    unknown: if unknown.is_empty() {
                        "no combination of its option realizations".to_owned()
                    } else {
                        unknown.join(", ")
                    },
                })
            })
            .collect(),
    }
}

/// The auxiliary artifacts a record's realized set adds to the binding's declared set.
fn added_artifacts(
    realized: &BTreeSet<String>,
    declared: &BTreeSet<String>,
) -> Option<BTreeSet<String>> {
    declared
        .is_subset(realized)
        .then(|| realized.difference(declared).cloned().collect())
}

/// Reserved series argument selecting every series a binding can realize.
const SERIES_ALL: &str = "all";

/// Reserved series argument selecting the series that adds no option-selected artifact.
const SERIES_NONE: &str = "none";

/// One series argument: the `all` keyword, or the artifacts a named series adds.
enum SeriesArgument {
    All,

    Named(BTreeSet<String>),
}

/// Parses one series argument, reserving `all` and `none` for the selection grammar.
fn parse_series(value: &str) -> Result<SeriesArgument, FitError> {
    if value == SERIES_ALL {
        return Ok(SeriesArgument::All);
    }
    if value == SERIES_NONE {
        return Ok(SeriesArgument::Named(BTreeSet::new()));
    }
    let mut artifacts = BTreeSet::new();
    for artifact in value.split(',') {
        if artifact.is_empty() {
            return Err(FitError::InvalidSeries(value.to_owned()));
        }
        artifacts.insert(artifact.to_owned());
    }
    Ok(SeriesArgument::Named(artifacts))
}

/// Renders each series with the number of records read for it.
fn series_records(counts: BTreeMap<BTreeSet<String>, usize>) -> Vec<SeriesRecords> {
    counts
        .into_iter()
        .map(|(series, record_count)| SeriesRecords {
            auxiliary_artifact_ids: series.into_iter().collect(),
            record_count,
        })
        .collect()
}

/// Renders the series a selection looked for, so a failure names them.
fn accepted_series_label(accepted: &BTreeSet<BTreeSet<String>>) -> String {
    format!(
        "the series {}",
        accepted
            .iter()
            .map(series_label)
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// Renders one series as the auxiliary artifacts it adds.
fn series_label(series: &BTreeSet<String>) -> String {
    format!("[{}]", series.iter().cloned().collect::<Vec<_>>().join(","))
}

/// One record's equation row, observed value, option choices and total context.
struct EvaluatedPoint {
    observed: u64,
    constant: f64,
    coefficients: Vec<f64>,
    choices: BTreeMap<String, String>,
    total_context: Option<u64>,
}

impl EvaluatedPoint {
    /// The observed value the fitted parameters must explain once the known terms are removed.
    fn target(&self) -> f64 {
        self.observed as f64 - self.constant
    }
}

fn evaluate(
    model: &SizingFitModel,
    configuration: &ModelEngineConfiguration,
    observed: &ObservedQuantity,
    record: &WorkloadResourceRecord,
) -> Result<EvaluatedPoint, String> {
    let value = observed
        .value(record)
        .ok_or_else(|| format!("record has no {}", observed.name()))?;
    let (engine_values, option_values) = record_inputs(configuration, record);
    let describe = |error| format!("{:#}", anyhow::Error::new(error));
    let point = model
        .point(&engine_values, &option_values, &fact_values(record))
        .map_err(describe)?
        .ok_or_else(|| "the estimate is unavailable for these inputs".to_owned())?;
    Ok(EvaluatedPoint {
        observed: value,
        constant: point.constant as f64,
        coefficients: point
            .coefficients
            .into_iter()
            .map(|value| value as f64)
            .collect(),
        choices: model.group(&option_values).map_err(describe)?,
        total_context: total_context(record),
    })
}

fn predict(
    model: &SizingFitModel,
    configuration: &ModelEngineConfiguration,
    solution: &BTreeMap<String, u64>,
    record: &WorkloadResourceRecord,
) -> Result<u64, String> {
    let (engine_values, option_values) = record_inputs(configuration, record);
    model
        .predict(
            solution,
            &engine_values,
            &option_values,
            &fact_values(record),
        )
        .map_err(|error| format!("{:#}", anyhow::Error::new(error)))
}

/// A record's engine parameter values and option values, completed with the configuration's
/// defaults for the options the record carries no value for.
fn record_inputs(
    configuration: &ModelEngineConfiguration,
    record: &WorkloadResourceRecord,
) -> (ParameterValues, Vec<ModelOptionValues>) {
    let (engine_values, mut option_values) = record
        .inputs
        .as_ref()
        .map(|inputs| {
            (
                inputs.engine_parameter_values.clone().unwrap_or_default(),
                inputs.option_values.clone(),
            )
        })
        .unwrap_or_default();
    let defaulted = defaulted_option_ids(configuration, record);
    option_values.extend(
        configuration
            .options
            .iter()
            .filter(|option| defaulted.contains(&option.option_id))
            .filter_map(|option| option.default_values.clone()),
    );
    (engine_values, option_values)
}

/// Options of the configuration the record carries no value for, in declaration order. Records
/// always carry every option their launch resolved, so a missing one was declared afterwards.
fn defaulted_option_ids(
    configuration: &ModelEngineConfiguration,
    record: &WorkloadResourceRecord,
) -> Vec<String> {
    let recorded = record
        .inputs
        .as_ref()
        .map(|inputs| inputs.option_values.as_slice())
        .unwrap_or_default();
    configuration
        .options
        .iter()
        .filter(|option| {
            !recorded
                .iter()
                .any(|values| values.option_id == option.option_id)
        })
        .map(|option| option.option_id.clone())
        .collect()
}

fn fact_values(record: &WorkloadResourceRecord) -> BTreeMap<String, u64> {
    let Some(EngineFacts::Vllm(facts)) = record
        .observation
        .as_ref()
        .and_then(|observation| observation.engine_facts.as_ref())
    else {
        return BTreeMap::new();
    };
    VllmFact::ALL
        .into_iter()
        .filter_map(|fact| Some((fact.symbol().to_owned(), fact.value(facts)?)))
        .collect()
}

/// Picks each realization's fit record: its startup record, else its first measurement.
fn fit_record_ids<'a>(
    records: impl Iterator<Item = &'a WorkloadResourceRecord>,
) -> BTreeSet<String> {
    let mut chosen = BTreeMap::<(String, u64), &WorkloadResourceRecord>::new();
    for record in records {
        let rank = fit_rank(record);
        if rank == 0 {
            continue;
        }
        chosen
            .entry(realization_key(record))
            .and_modify(|current| {
                if (rank, recorded_at(record)) > (fit_rank(current), recorded_at(current)) {
                    *current = record;
                }
            })
            .or_insert(record);
    }
    chosen
        .into_values()
        .map(|record| record.record_id.clone())
        .collect()
}

/// Startup records win over measurements; peaks never become fit points.
fn fit_rank(record: &WorkloadResourceRecord) -> u8 {
    match record
        .observation
        .as_ref()
        .and_then(|observation| WorkloadResourceObservationKind::try_from(observation.kind).ok())
    {
        Some(WorkloadResourceObservationKind::Startup) => 2,
        Some(WorkloadResourceObservationKind::Measurement) => 1,
        _ => 0,
    }
}

fn recorded_at(record: &WorkloadResourceRecord) -> std::cmp::Reverse<(i64, i32)> {
    let timestamp = record.recorded_at.unwrap_or_default();
    std::cmp::Reverse((timestamp.seconds, timestamp.nanos))
}

fn realization_key(record: &WorkloadResourceRecord) -> (String, u64) {
    record
        .realization
        .as_ref()
        .map(|key| (key.owner_id.clone(), key.generation))
        .unwrap_or_else(|| (record.record_id.clone(), 0))
}

fn point_report(
    record: &WorkloadResourceRecord,
    choices: &BTreeMap<String, String>,
    fitted: bool,
    observed: u64,
    predicted: u64,
) -> FitPointReport {
    let identity = point_identity(record);
    FitPointReport {
        record_id: record.record_id.clone(),
        owner_id: identity.owner_id,
        generation: identity.generation,
        node_id: identity.node_id,
        accelerator: identity.accelerator,
        kind: identity.kind,
        choices: choices.clone(),
        fitted,
        observed,
        predicted,
        residual: signed_difference(observed, predicted),
    }
}

/// Where a record comes from: its realization, node, first accelerator and observation kind.
struct PointIdentity {
    owner_id: String,
    generation: u64,
    node_id: String,
    accelerator: String,
    kind: &'static str,
}

fn point_identity(record: &WorkloadResourceRecord) -> PointIdentity {
    let (owner_id, generation) = realization_key(record);
    let node = record.node.as_ref();
    PointIdentity {
        owner_id,
        generation,
        node_id: node.map(|node| node.node_id.clone()).unwrap_or_default(),
        accelerator: node
            .and_then(|node| node.devices.first())
            .map(|device| device.model.clone())
            .unwrap_or_default(),
        kind: record
            .observation
            .as_ref()
            .and_then(|observation| {
                WorkloadResourceObservationKind::try_from(observation.kind).ok()
            })
            .map_or("unspecified", |kind| match kind {
                WorkloadResourceObservationKind::Startup => "startup",
                WorkloadResourceObservationKind::Peak => "peak",
                WorkloadResourceObservationKind::Measurement => "measurement",
                WorkloadResourceObservationKind::Unspecified => "unspecified",
            }),
    }
}

/// The record's total context engine value, when it carries one.
fn total_context(record: &WorkloadResourceRecord) -> Option<u64> {
    record
        .inputs
        .as_ref()?
        .engine_parameter_values
        .as_ref()?
        .values
        .iter()
        .find(|value| value.parameter_id == TOTAL_CONTEXT_PARAMETER_ID)
        .and_then(|value| match value.value {
            Some(parameter_value::Value::UnsignedInteger(value)) => Some(value),
            _ => None,
        })
}

/// `left - right`, saturated to the signed range.
fn signed_difference(left: u64, right: u64) -> i64 {
    (i128::from(left) - i128::from(right)).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

fn solvable_hint(solvable: &[String]) -> String {
    if solvable.is_empty() {
        "no parameter can be fitted from these points".to_owned()
    } else {
        format!("fitting only {} would solve it", solvable.join(", "))
    }
}

fn residual_summaries(points: &[FitPointReport]) -> BTreeMap<String, ResidualSummary> {
    let mut by_accelerator = BTreeMap::<String, Vec<i64>>::new();
    for point in points {
        by_accelerator
            .entry(point.accelerator.clone())
            .or_default()
            .push(point.residual);
    }
    by_accelerator
        .into_iter()
        .filter_map(|(accelerator, residuals)| {
            Some((accelerator, ResidualSummary::of(&residuals)?))
        })
        .collect()
}

fn warm_growth<'a>(
    records: impl Iterator<Item = &'a WorkloadResourceRecord>,
    fit_records: &BTreeSet<String>,
) -> WarmGrowthReport {
    let mut realizations = BTreeMap::<(String, u64), (Option<u64>, u64)>::new();
    for record in records {
        let bytes = record
            .observation
            .as_ref()
            .map_or(0, |observation| observation.attributed_memory_bytes);
        let entry = realizations
            .entry(realization_key(record))
            .or_insert((None, 0));
        if fit_records.contains(&record.record_id) {
            entry.0 = Some(bytes);
        }
        entry.1 = entry.1.max(bytes);
    }
    let realizations = realizations
        .into_iter()
        .filter_map(|((owner_id, generation), (fit_point, maximum))| {
            let fit_point_bytes = fit_point?;
            Some(RealizationGrowth {
                owner_id,
                generation,
                fit_point_bytes,
                maximum_bytes: maximum,
                growth_bytes: maximum.saturating_sub(fit_point_bytes),
            })
        })
        .collect::<Vec<_>>();
    WarmGrowthReport {
        observational: true,
        maximum_growth_bytes: realizations
            .iter()
            .map(|growth| growth.growth_bytes)
            .max()
            .unwrap_or_default(),
        realizations,
    }
}

#[cfg(test)]
mod tests;
