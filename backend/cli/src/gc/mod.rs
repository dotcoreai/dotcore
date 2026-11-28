//! `dotc gc`: reclaims disk on this machine that nothing uses any more.
//!
//! It removes Docker images no Docker-delivered Nix artifact uses, for example an n8n image left
//! behind by an older loader once no node pins it, then shared workload storage that no workload
//! has used for a number of days. Each step lists what it would remove and asks for confirmation
//! before removing it. It acts on the whole machine, so it needs neither a running server nor a
//! site, and it never adds Nix roots or runs Nix garbage collection. It prints
//! tables, on the terminal or as plain text without one. Asked for YAML, it
//! never asks, so it needs confirmation in advance, and prints one document of every removal.

mod storage;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Args;
use dotcore_artifact::{
    DockerOciClient, ImageCleanup, ImageRemoval, RemovalOutcome, RemovalReason,
};
use serde::Serialize;

use crate::{
    RootPathArgs,
    output::{LazyTerminal, OutputFormat, present_on, write_yaml},
    terminal::{ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition},
};

/// Arguments of `dotc gc`.
#[derive(Debug, Args)]
pub struct GcCommand {
    /// Remove the image names without asking for confirmation. Required when there is something
    /// to remove and no terminal to ask on.
    #[arg(long)]
    yes: bool,

    /// Remove shared workload storage, such as engine caches or application data kept across
    /// deployments, that no workload has used for this many days.
    #[arg(long, value_name = "DAYS", default_value_t = 7)]
    unused_days: u32,
}

impl GcCommand {
    pub(crate) async fn dispatch(
        self,
        paths: &RootPathArgs,
        format: OutputFormat,
    ) -> anyhow::Result<()> {
        let images = self.clean_images(paths, format).await?;
        let storage = storage::clean(paths, self.unused_days, self.yes, format).await?;
        if format == OutputFormat::Yaml {
            write_yaml(&GcDocument { images, storage })?;
        }
        Ok(())
    }

    /// Removes the unused Docker image names and returns what happened to each, or none when the
    /// operator declined or there was nothing to remove.
    async fn clean_images(
        &self,
        paths: &RootPathArgs,
        format: OutputFormat,
    ) -> anyhow::Result<Vec<ImageResult>> {
        let cache = dotcore_domain::CacheLayout(&paths.cache_root()?).model_artifacts();
        let docker = DockerOciClient::connect()
            .await
            .context("cannot reach the Docker daemon")?;
        let cleanup = ImageCleanup::new(&cache, Arc::new(docker));
        let planned = cleanup
            .plan()
            .await
            .context("Docker image cleanup planning failed")?;
        let mut terminal = LazyTerminal::new();
        if !approve_removals(&mut terminal, &planned, self.yes, format)? {
            return Ok(Vec::new());
        }

        let results = cleanup
            .apply()
            .await
            .context("Docker image cleanup failed")?;
        if format == OutputFormat::Human {
            show_results(&mut terminal, &results)?;
        }
        let failed = results
            .iter()
            .filter(|(_, outcome)| matches!(outcome, RemovalOutcome::Failed(_)))
            .count();
        anyhow::ensure!(failed == 0, "{failed} Docker image removals failed");
        Ok(results
            .iter()
            .map(|(removal, outcome)| ImageResult::new(removal, outcome))
            .collect())
    }
}

/// The YAML document of `dotc gc`: what each cleanup step removed.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GcDocument {
    images: Vec<ImageResult>,
    storage: Vec<storage::StorageResult>,
}

/// What happened to one Docker image name.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageResult {
    image_id: String,
    reference: String,
    reason: &'static str,
    result: &'static str,
}

impl ImageResult {
    fn new(removal: &ImageRemoval, outcome: &RemovalOutcome) -> Self {
        Self {
            image_id: removal.image_id.clone(),
            reference: removal.reference.clone(),
            reason: match removal.reason {
                RemovalReason::UnusedName => "unusedName",
                RemovalReason::DigestReference => "digestReference",
                RemovalReason::UntaggedBuild => "untaggedBuild",
            },
            result: match outcome {
                RemovalOutcome::Removed => "removed",
                RemovalOutcome::InUse => "inUse",
                RemovalOutcome::Failed(_) => "failed",
            },
        }
    }
}

/// Decides whether the planned image removals go ahead, as [`approve`] does.
fn approve_removals(
    terminal: &mut dyn Terminal,
    planned: &[ImageRemoval],
    yes: bool,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    approve(
        terminal,
        Approval {
            nothing: "No Docker image to clean up",
            noun: "Docker image names",
            header: &["IMAGE", "NAME", "REASON"],
            rows: planned.iter().map(removal_row).collect(),
        },
        yes,
        format,
    )
}

/// The removals one cleanup step asks the operator to approve.
struct Approval<'a> {
    /// The message shown when there is nothing to remove.
    nothing: &'a str,
    /// What the removals are, in the plural.
    noun: &'a str,
    header: &'a [&'a str],
    rows: Vec<Vec<String>>,
}

/// Decides whether the planned removals go ahead: nothing to remove needs no approval, `yes`
/// approves in advance, and otherwise the operator confirms the listed removals on the terminal.
/// Without a terminal, or asked for YAML, it refuses, and a declined prompt returns
/// [`PromptError::Cancelled`].
fn approve(
    terminal: &mut dyn Terminal,
    approval: Approval<'_>,
    yes: bool,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    let count = approval.rows.len();
    if count == 0 {
        if format == OutputFormat::Human {
            present_on(terminal, &[Message::Success(approval.nothing.to_owned())])?;
        }
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    if format == OutputFormat::Yaml {
        anyhow::bail!(
            "{count} {} can be removed; --yaml never asks for confirmation, pass --yes to remove them",
            approval.noun
        );
    }
    if terminal.condition() == TerminalCondition::NonInteractive {
        anyhow::bail!(
            "{count} {} can be removed; run in a terminal or pass --yes to remove them without confirmation",
            approval.noun
        );
    }

    terminal.prompt_context(&Message::Table {
        header: approval
            .header
            .iter()
            .map(|&title| title.to_owned())
            .collect(),
        rows: approval.rows,
    })?;
    let question = format!("Remove these {count} {}?", approval.noun);
    if terminal.confirm(&ConfirmPrompt::new(question, false))? {
        Ok(true)
    } else {
        Err(PromptError::Cancelled.into())
    }
}

/// Shows each removal with its outcome on the terminal, then points to Nix garbage collection,
/// which reclaims store space on its own schedule.
fn show_results(
    terminal: &mut dyn Terminal,
    results: &[(ImageRemoval, RemovalOutcome)],
) -> anyhow::Result<()> {
    present_on(
        terminal,
        &[Message::Table {
            header: ["IMAGE", "NAME", "REASON", "RESULT"]
                .map(str::to_owned)
                .to_vec(),
            rows: results
                .iter()
                .map(|(removal, outcome)| {
                    let mut row = removal_row(removal);
                    row.push(outcome_text(outcome));
                    row
                })
                .collect(),
        }],
    )?;
    let removed = results
        .iter()
        .filter(|(_, outcome)| outcome == &RemovalOutcome::Removed)
        .count();
    present_on(
        terminal,
        &[Message::Success(format!(
            "Removed {removed} of {} Docker image names",
            results.len()
        ))],
    )?;
    present_on(
        terminal,
        &[Message::Info(
            "Nix store space is reclaimed separately by Nix garbage collection".to_owned(),
        )],
    )?;
    present_on(terminal, &[Message::Command("nix-store --gc".to_owned())])?;
    Ok(())
}

fn removal_row(removal: &ImageRemoval) -> Vec<String> {
    vec![
        short_id(&removal.image_id).to_owned(),
        removal.reference.clone(),
        reason(removal.reason).to_owned(),
    ]
}

fn reason(reason: RemovalReason) -> &'static str {
    match reason {
        RemovalReason::UnusedName => "unused name",
        RemovalReason::DigestReference => "digest reference",
        RemovalReason::UntaggedBuild => "untagged build",
    }
}

fn outcome_text(outcome: &RemovalOutcome) -> String {
    match outcome {
        RemovalOutcome::Removed => "removed".to_owned(),
        RemovalOutcome::InUse => "in use".to_owned(),
        RemovalOutcome::Failed(detail) => format!("failed: {detail}"),
    }
}

/// Returns the short image ID Docker shows, without its algorithm.
fn short_id(image_id: &str) -> &str {
    let id = image_id.strip_prefix("sha256:").unwrap_or(image_id);
    &id[..id.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

    #[test]
    fn the_short_image_id_drops_the_algorithm() {
        assert_eq!(short_id(&n8n_removal().image_id), "0123456789ab");
    }

    #[test]
    fn nothing_to_remove_needs_no_approval() {
        let mut terminal = ScriptedTerminal::interactive([]);

        assert!(
            !approve_removals(&mut terminal, &[], false, OutputFormat::Human)
                .expect("nothing to approve")
        );
        assert_eq!(terminal.lines(), ["✔ No Docker image to clean up"]);
    }

    #[test]
    fn removals_without_a_terminal_need_yes() {
        let planned = [n8n_removal()];
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error = approve_removals(&mut terminal, &planned, false, OutputFormat::Human)
            .expect_err("needs yes");

        assert_eq!(
            error.to_string(),
            "1 Docker image names can be removed; run in a terminal or pass --yes to remove them without confirmation"
        );
        assert!(
            approve_removals(&mut terminal, &planned, true, OutputFormat::Human)
                .expect("yes approves")
        );
        assert!(terminal.prompts().is_empty());
    }

    #[test]
    fn yaml_never_asks_so_removals_need_yes_and_nothing_to_remove_prints_nothing() {
        let planned = [n8n_removal()];
        let mut terminal = ScriptedTerminal::interactive([]);

        let error = approve_removals(&mut terminal, &planned, false, OutputFormat::Yaml)
            .expect_err("needs yes");

        assert_eq!(
            error.to_string(),
            "1 Docker image names can be removed; --yaml never asks for confirmation, pass --yes to remove them"
        );
        assert!(terminal.prompts().is_empty());
        assert!(terminal.messages().is_empty());
        assert!(
            approve_removals(&mut terminal, &planned, true, OutputFormat::Yaml)
                .expect("yes approves")
        );
        assert!(
            !approve_removals(&mut terminal, &[], false, OutputFormat::Yaml)
                .expect("nothing to approve")
        );
        assert!(terminal.messages().is_empty());
    }

    #[test]
    fn the_document_names_each_removal_and_its_result() {
        let document = GcDocument {
            images: vec![
                ImageResult::new(&n8n_removal(), &RemovalOutcome::Removed),
                ImageResult::new(&n8n_removal(), &RemovalOutcome::InUse),
            ],
            storage: Vec::new(),
        };

        assert_eq!(
            serde_json::to_value(&document).expect("document"),
            serde_json::json!({
                "images": [
                    {
                        "imageId": "sha256:0123456789abcdef",
                        "reference": "dotcore-nix/docker-image-n8n:0123",
                        "reason": "unusedName",
                        "result": "removed",
                    },
                    {
                        "imageId": "sha256:0123456789abcdef",
                        "reference": "dotcore-nix/docker-image-n8n:0123",
                        "reason": "unusedName",
                        "result": "inUse",
                    },
                ],
                "storage": [],
            })
        );
    }

    #[test]
    fn the_operator_confirms_the_listed_removals() {
        let planned = [n8n_removal()];
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Confirm(true),
            ScriptedAnswer::Confirm(false),
        ]);

        assert!(
            approve_removals(&mut terminal, &planned, false, OutputFormat::Human)
                .expect("confirmed")
        );
        let declined = approve_removals(&mut terminal, &planned, false, OutputFormat::Human)
            .expect_err("declined");

        assert!(matches!(
            declined.downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        let question = ScriptedPrompt::Confirm(ConfirmPrompt::new(
            "Remove these 1 Docker image names?",
            false,
        ));
        assert_eq!(terminal.prompts(), [question.clone(), question]);
        assert_eq!(
            terminal.messages()[0],
            Message::Table {
                header: ["IMAGE", "NAME", "REASON"].map(str::to_owned).to_vec(),
                rows: vec![vec![
                    "0123456789ab".to_owned(),
                    "dotcore-nix/docker-image-n8n:0123".to_owned(),
                    "unused name".to_owned(),
                ]],
            }
        );
    }

    fn n8n_removal() -> ImageRemoval {
        ImageRemoval {
            image_id: "sha256:0123456789abcdef".to_owned(),
            reference: "dotcore-nix/docker-image-n8n:0123".to_owned(),
            reason: RemovalReason::UnusedName,
        }
    }
}
