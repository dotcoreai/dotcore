//! Workload resource telemetry commands, exposed as `dotc dev telemetry`.
//!
//! `fit` reads the records of one or more telemetry directories, for example one copied from each
//! host, and fits a model binding's sizing equation on one hardware class: one joint solution,
//! one fit per group of option choices, and the constants proposed for the binding with their
//! margin and a block ready to paste. `check` compares the stored constants with the same records.
//! Both print one JSON envelope and change nothing.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use dotcore_backend::telemetry::{
    WorkloadResourceRecords,
    fit::{FitEquation, FitRequest, ObservedQuantity, SeriesSelection, check, fit},
};
use dotcore_catalog::{Catalog, OciArtifactPolicy};
use serde_json::json;

use crate::RootPathArgs;

const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub(crate) struct TelemetryCommand {
    #[command(subcommand)]
    command: Option<TelemetryAction>,
}

#[derive(Debug, Subcommand)]
enum TelemetryAction {
    /// Fit a model binding's sizing constants on one hardware class from recorded workload resources.
    Fit(FitArgs),

    /// Check a model binding's stored sizing constants on one hardware class against recorded workload resources.
    Check(CheckArgs),
}

#[derive(Debug, Args)]
struct FitArgs {
    #[command(flatten)]
    selection: SelectionArgs,
    /// Ad hoc equation to fit instead of a catalog estimate; may use `${vllm.<fact>}`.
    #[arg(long, conflicts_with = "estimate")]
    equation: Option<String>,
    /// Parameter to fit; repeat for each. Defaults to the binding constants the estimate's equation
    /// uses, leaving out those no record exercises.
    #[arg(long = "parameter", action = clap::ArgAction::Append)]
    parameters: Vec<String>,
}

#[derive(Debug, Args)]
struct CheckArgs {
    #[command(flatten)]
    selection: SelectionArgs,
}

/// Records, catalog, and binding a fit or check reads.
#[derive(Debug, Args)]
struct SelectionArgs {
    /// Telemetry directory to read; repeat to merge hosts.
    #[arg(long = "telemetry-dir", required = true, action = clap::ArgAction::Append)]
    telemetry_dirs: Vec<PathBuf>,
    #[arg(long, env = "DOTCORE_DEV_CATALOG_DIR")]
    development_catalog: Option<PathBuf>,
    #[arg(long)]
    model_id: String,
    #[arg(long)]
    configuration_id: String,
    /// Supported hardware class of the configuration whose sizing to read; records that name no
    /// hardware class are left out.
    #[arg(long)]
    hardware_class_id: String,
    #[arg(long)]
    binding_id: String,
    /// Catalog estimate of the binding.
    #[arg(long, default_value = "acceleratorMemory")]
    estimate: String,
    /// Value to predict: attributed-bytes, vllm.<fact>, or engine.<parameter>.
    #[arg(long, default_value = "attributed-bytes")]
    observed: String,
    /// Series of the binding to read: `none`, `all`, or comma-separated auxiliary artifact ids.
    /// Repeat once per named series.
    #[arg(long = "series", action = clap::ArgAction::Append, value_name = "ARTIFACT_IDS|ALL")]
    series: Vec<String>,
    /// Read records launched with custom arguments too; they are skipped and listed by default.
    #[arg(long)]
    include_custom_argument_records: bool,
}

impl TelemetryCommand {
    pub(crate) async fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        match self.command {
            Some(TelemetryAction::Fit(args)) => fit_command(paths, args),
            Some(TelemetryAction::Check(args)) => check_command(paths, args),
            None => bail!("telemetry command requires an action"),
        }
    }
}

fn fit_command(paths: &RootPathArgs, args: FitArgs) -> Result<()> {
    if args.equation.is_some() && args.parameters.is_empty() {
        bail!("an inline --equation requires at least one --parameter to fit");
    }
    let equation = match args.equation {
        Some(equation) => FitEquation::Inline(equation),
        None => FitEquation::Estimate(args.selection.estimate.clone()),
    };
    let request = request(&args.selection, equation, args.parameters)?;
    let (catalog, records) = read_inputs(paths, &args.selection)?;
    let report = fit(&catalog, &request, &records.records).context("fit sizing constants")?;
    print_envelope("fit", &records, json!(report));
    Ok(())
}

fn check_command(paths: &RootPathArgs, args: CheckArgs) -> Result<()> {
    let equation = FitEquation::Estimate(args.selection.estimate.clone());
    let request = request(&args.selection, equation, Vec::new())?;
    let (catalog, records) = read_inputs(paths, &args.selection)?;
    let report =
        check(&catalog, &request, &records.records).context("check stored sizing constants")?;
    print_envelope("check", &records, json!(report));
    Ok(())
}

fn request(
    selection: &SelectionArgs,
    equation: FitEquation,
    parameters: Vec<String>,
) -> Result<FitRequest> {
    Ok(FitRequest {
        model_id: selection.model_id.clone(),
        configuration_id: selection.configuration_id.clone(),
        hardware_class_id: selection.hardware_class_id.clone(),
        binding_id: selection.binding_id.clone(),
        equation,
        parameters,
        observed: selection.observed.parse::<ObservedQuantity>()?,
        series: SeriesSelection::from_values(&selection.series)?,
        include_custom_argument_records: selection.include_custom_argument_records,
    })
}

/// Loads the catalog and reads the telemetry records, reporting malformed lines on stderr.
fn read_inputs(
    paths: &RootPathArgs,
    selection: &SelectionArgs,
) -> Result<(Catalog, WorkloadResourceRecords)> {
    let master_catalog = paths.master_catalog()?;
    let validator = dotcore_backend::model::engine::declaration_validator();
    let catalog = match &selection.development_catalog {
        Some(development) => Catalog::load_sources_with_policy(
            &master_catalog,
            development,
            OciArtifactPolicy::Development,
            validator,
        ),
        None => {
            Catalog::load_with_policy(&master_catalog, OciArtifactPolicy::Development, validator)
        }
    }
    .context("load catalog for the telemetry read")?;
    let records = WorkloadResourceRecords::read(&selection.telemetry_dirs)
        .context("read telemetry records")?;
    for malformed in &records.malformed {
        eprintln!(
            "skipped malformed telemetry line {}:{}: {}",
            malformed.path.display(),
            malformed.line,
            malformed.message
        );
    }
    Ok((catalog, records))
}

/// Prints the success envelope, with the report under the operation's name.
fn print_envelope(operation: &str, records: &WorkloadResourceRecords, report: serde_json::Value) {
    let mut payload = json!({
        "record_count": records.records.len(),
        "malformed_line_count": records.malformed.len(),
    });
    payload[operation] = report;
    println!(
        "{}",
        json!({
            "format_version": FORMAT_VERSION,
            "operation": operation,
            "result": "success",
            "payload": payload,
        })
    );
}
