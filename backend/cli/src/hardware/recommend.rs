//! `dotc hardware recommend`: ranks the master catalog's models for a machine.
//!
//! The machine is this one or a machine description file, treated as idle. For each use case it
//! prints a table of recommended models, best first, with the execution target that runs each,
//! whether it fits there now, and the total context it starts with.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use dotcore::model::{IdleMachine, RecommendationFit, UseCaseRecommendations};
use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::catalog::v1::UseCase;

use super::load_catalog;
use crate::{RootPathArgs, terminal::Message};

/// Arguments of `dotc hardware recommend`.
#[derive(Debug, Args)]
pub(super) struct RecommendArgs {
    /// A machine description file (the output of `dotc hardware show`) to recommend for, instead
    /// of this machine.
    #[arg(long, value_name = "FILE")]
    machine: Option<PathBuf>,

    /// Recommend for one use case only; all use cases by default.
    #[arg(long, value_enum)]
    use_case: Option<UseCaseArg>,
}

/// The use cases a recommendation can be asked for.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum UseCaseArg {
    Coding,
    Assistant,
    Translation,
    Decision,
}

impl UseCaseArg {
    const ALL: [Self; 4] = [
        Self::Coding,
        Self::Assistant,
        Self::Translation,
        Self::Decision,
    ];

    fn use_case(self) -> UseCase {
        match self {
            Self::Coding => UseCase::CodingAndAgents,
            Self::Assistant => UseCase::GeneralAssistant,
            Self::Translation => UseCase::Translation,
            Self::Decision => UseCase::DecisionMaking,
        }
    }
}

/// Prints the recommendations for the machine on stdout.
pub(super) fn recommend(paths: &RootPathArgs, arguments: RecommendArgs) -> Result<()> {
    let catalog = load_catalog(&paths.master_catalog()?)?;
    let machine = match &arguments.machine {
        Some(file) => read_machine(file)?,
        None => MachineDescription::from_local_host()
            .context("cannot describe the hardware of this machine")?,
    };
    let use_cases = arguments
        .use_case
        .map_or(UseCaseArg::ALL.to_vec(), |one| vec![one])
        .into_iter()
        .map(UseCaseArg::use_case)
        .collect::<Vec<_>>();

    let idle = IdleMachine::new(&machine).context("cannot work out the execution targets")?;
    let recommendations = idle.recommend(&catalog, &use_cases);
    print!("{}", render(&catalog, &idle, &recommendations));
    Ok(())
}

/// Reads a machine description from a YAML file.
fn read_machine(file: &PathBuf) -> Result<MachineDescription> {
    let yaml = std::fs::read_to_string(file)
        .with_context(|| format!("cannot read the machine description {}", file.display()))?;
    let name = file
        .file_stem()
        .map_or("machine".into(), |stem| stem.to_string_lossy());
    MachineDescription::from_yaml(&name, &yaml)
        .with_context(|| format!("invalid machine description {}", file.display()))
}

/// Lays the recommendations out with the shared terminal layout: a heading and a table per use
/// case, or a line saying nothing is recommended.
fn render(
    catalog: &Catalog,
    machine: &IdleMachine,
    recommendations: &[UseCaseRecommendations],
) -> String {
    let mut text = String::new();
    for (index, list) in recommendations.iter().enumerate() {
        if index > 0 {
            text.push('\n');
        }
        let heading = Message::Heading(use_case_label(list.use_case).to_owned());
        let body = if list.rows.is_empty() {
            Message::Info("No recommendation for this machine".to_owned())
        } else {
            table(catalog, machine, list)
        };
        for message in [heading, body] {
            for line in message.plain_lines() {
                text.push_str(&line);
                text.push('\n');
            }
        }
    }
    text
}

/// Builds one use case's table: a row per recommended model and execution target.
fn table(catalog: &Catalog, machine: &IdleMachine, list: &UseCaseRecommendations) -> Message {
    let rows = list
        .rows
        .iter()
        .flat_map(|row| {
            let model = model_name(catalog, &row.model_id);
            let profile = profile_name(catalog, &row.model_id, &row.profile_id);
            row.targets.iter().map(move |target| {
                vec![
                    model.clone(),
                    profile.clone(),
                    target_name(machine, &target.placement.execution_target_id),
                    fit_label(target.fit).to_owned(),
                    target
                        .starting_total_context_tokens
                        .map_or("-".to_owned(), |tokens| tokens.to_string()),
                ]
            })
        })
        .collect();
    Message::Table {
        header: ["MODEL", "PROFILE", "TARGET", "STATUS", "CONTEXT TOKENS"]
            .map(str::to_owned)
            .to_vec(),
        rows,
    }
}

fn use_case_label(use_case: UseCase) -> &'static str {
    match use_case {
        UseCase::CodingAndAgents => "Coding and agents",
        UseCase::GeneralAssistant => "General assistant",
        UseCase::Translation => "Translation",
        UseCase::DecisionMaking => "Decision making",
        UseCase::Unspecified => "Unspecified",
    }
}

fn fit_label(fit: RecommendationFit) -> &'static str {
    match fit {
        RecommendationFit::FitsNow => "fits now",
        RecommendationFit::NeedsMemoryFreed => "needs memory freed",
        RecommendationFit::Unknown => "fit unknown",
    }
}

fn model_name(catalog: &Catalog, model_id: &str) -> String {
    catalog
        .list_model_releases()
        .into_iter()
        .find(|model| model.model_id == model_id)
        .map(|model| model.display_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| model_id.to_owned())
}

fn profile_name(catalog: &Catalog, model_id: &str, profile_id: &str) -> String {
    catalog
        .profiles_for_model(&CatalogModelId(model_id.to_owned()))
        .into_iter()
        .find(|profile| profile.profile_id == profile_id)
        .map(|profile| profile.display_name)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| profile_id.to_owned())
}

fn target_name(machine: &IdleMachine, execution_target_id: &str) -> String {
    machine
        .targets()
        .iter()
        .find(|target| target.execution_target_id == execution_target_id)
        .map_or_else(
            || execution_target_id.to_owned(),
            |target| target.display_name.clone(),
        )
}
