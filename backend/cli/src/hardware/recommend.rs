//! `dotc hardware recommend`: ranks the master catalog's models for a machine.
//!
//! The machine is this one or a machine description file, treated as idle. For each use case it
//! prints a table of recommended models, best first, with the execution target that runs each,
//! whether it fits there now, its context per request and the total context it starts with. Each
//! use case also names the catalog application that serves it, and says when no target runs it
//! or none holds the accelerators it prefers.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use dotcore::model::{IdleMachine, RecommendationFit, UseCaseRecommendations};
use dotcore_catalog::{Catalog, CatalogModelId};
use dotcore_domain::timing::Timing;
use dotcore_node_hardware::MachineDescription;
use dotcore_proto::dotcore::catalog::v1::{Application, UseCase};

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
    Chat,
    Image,
}

impl UseCaseArg {
    const ALL: [Self; 3] = [Self::Coding, Self::Chat, Self::Image];

    fn use_case(self) -> UseCase {
        match self {
            Self::Coding => UseCase::Coding,
            Self::Chat => UseCase::ChatAssistant,
            Self::Image => UseCase::ImageGeneration,
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

    let idle = IdleMachine::new(&machine, Timing::for_process().node_control)
        .context("cannot work out the execution targets")?;
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

/// Lays the recommendations out with the shared terminal layout: a heading per use case, then its
/// table of models and the application that serves it, or a line saying nothing is recommended.
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
        let application = catalog
            .list_application_releases()
            .into_iter()
            .find(|application| application.use_cases().any(|case| case == list.use_case));
        let mut messages = vec![heading];
        if !list.rows.is_empty() {
            messages.push(table(catalog, machine, list));
        }
        if let Some(application) = application {
            messages.push(application_line(machine, &application));
        }
        if messages.len() == 1 {
            messages.push(Message::Info(
                "No recommendation for this machine".to_owned(),
            ));
        }
        for message in messages {
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
                    row.context_tokens_per_request.to_string(),
                    target
                        .starting_total_context_tokens
                        .map_or("-".to_owned(), |tokens| tokens.to_string()),
                ]
            })
        })
        .collect();
    Message::Table {
        header: [
            "MODEL",
            "PROFILE",
            "TARGET",
            "STATUS",
            "CONTEXT PER REQUEST",
            "TOTAL CONTEXT",
        ]
        .map(str::to_owned)
        .to_vec(),
        rows,
    }
}

/// Names the application that serves a use case, with a note when the machine can't run it or
/// holds none of the accelerators it prefers.
fn application_line(machine: &IdleMachine, application: &Application) -> Message {
    let options = machine.recommend_application(application);
    let note = if options.is_empty() {
        Some("no target can run it".to_owned())
    } else if !options.iter().any(|option| option.matches_preference) {
        Some(format!(
            "no target matches its preferred accelerators: {}",
            application.preferred_accelerators.join(", ")
        ))
    } else {
        None
    };
    let name = match note {
        Some(note) => format!("{} ({note})", application.display_name),
        None => application.display_name.clone(),
    };
    Message::Info(format!("App: {name}"))
}

fn use_case_label(use_case: UseCase) -> &'static str {
    match use_case {
        UseCase::Coding => "Coding",
        UseCase::ChatAssistant => "Chat assistant",
        UseCase::ImageGeneration => "Image generation",
        UseCase::Unspecified => "Unspecified",
    }
}

fn fit_label(fit: RecommendationFit) -> &'static str {
    match fit {
        RecommendationFit::FitsNow => "fits now",
        RecommendationFit::NeedsMemoryFreed => "needs memory freed",
        RecommendationFit::Unknown => "fit unknown",
        RecommendationFit::Unreachable => "unreachable",
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
