//! `dotc gc`: reclaims disk on this machine that nothing uses any more.
//!
//! It removes Docker images no Docker-delivered Nix artifact uses, for example an n8n image left
//! behind by an older loader once no node pins it, then shared workload storage that no workload
//! has used for a number of days. Each step lists what it would remove and asks for confirmation
//! before removing it. It acts on the whole machine, so it needs neither a running server nor a
//! site, and it never adds Nix roots or runs Nix garbage collection. On a terminal it prints
//! tables; without one it prints one record block per removal on stdout.

mod storage;

use std::sync::Arc;

use anyhow::Context as _;
use clap::Args;
use dotcore_artifact::{
    DockerOciClient, ImageCleanup, ImageRemoval, RemovalOutcome, RemovalReason,
};

use crate::{
    RootPathArgs,
    node::{record_value, write_records},
    terminal::{
        ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition,
        controlling_terminal_condition, open_terminal,
    },
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
    pub(crate) async fn dispatch(self, paths: &RootPathArgs) -> anyhow::Result<()> {
        self.clean_images(paths).await?;
        storage::clean(paths, self.unused_days, self.yes).await
    }

    async fn clean_images(&self, paths: &RootPathArgs) -> anyhow::Result<()> {
        let cache = dotcore_domain::CacheLayout(&paths.cache_root()?).model_artifacts();
        let docker = DockerOciClient::connect()
            .await
            .context("cannot reach the Docker daemon")?;
        let cleanup = ImageCleanup::new(&cache, Arc::new(docker));
        let planned = cleanup
            .plan()
            .await
            .context("Docker image cleanup planning failed")?;
        let mut terminal = open_terminal();
        if !approve_removals(terminal.as_mut(), &planned, self.yes)? {
            return Ok(());
        }

        let results = cleanup
            .apply()
            .await
            .context("Docker image cleanup failed")?;
        if controlling_terminal_condition() == TerminalCondition::Interactive {
            show_results(terminal.as_mut(), &results)?;
        } else {
            let blocks = results
                .iter()
                .map(|(removal, outcome)| record_block(removal, outcome))
                .collect::<anyhow::Result<Vec<_>>>()?;
            write_records(&blocks.join("\n"))?;
        }
        let failed = results
            .iter()
            .filter(|(_, outcome)| matches!(outcome, RemovalOutcome::Failed(_)))
            .count();
        anyhow::ensure!(failed == 0, "{failed} Docker image removals failed");
        Ok(())
    }
}

/// Decides whether the planned image removals go ahead, as [`approve`] does.
fn approve_removals(
    terminal: &mut dyn Terminal,
    planned: &[ImageRemoval],
    yes: bool,
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
/// Without a terminal it refuses, and a declined prompt returns [`PromptError::Cancelled`].
fn approve(terminal: &mut dyn Terminal, approval: Approval<'_>, yes: bool) -> anyhow::Result<bool> {
    let count = approval.rows.len();
    if count == 0 {
        terminal.message(&Message::Success(approval.nothing.to_owned()))?;
        return Ok(false);
    }
    if yes {
        return Ok(true);
    }
    if terminal.condition() == TerminalCondition::NonInteractive {
        anyhow::bail!(
            "{count} {} can be removed; run in a terminal or pass --yes to remove them without confirmation",
            approval.noun
        );
    }

    terminal.message(&Message::Table {
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
    terminal.message(&Message::Table {
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
    })?;
    let removed = results
        .iter()
        .filter(|(_, outcome)| outcome == &RemovalOutcome::Removed)
        .count();
    terminal.message(&Message::Success(format!(
        "Removed {removed} of {} Docker image names",
        results.len()
    )))?;
    terminal.message(&Message::Info(
        "Nix store space is reclaimed separately by Nix garbage collection".to_owned(),
    ))?;
    terminal.message(&Message::Command("nix-store --gc".to_owned()))?;
    Ok(())
}

fn removal_row(removal: &ImageRemoval) -> Vec<String> {
    vec![
        short_id(&removal.image_id).to_owned(),
        removal.reference.clone(),
        reason(removal.reason).to_owned(),
    ]
}

fn record_block(removal: &ImageRemoval, outcome: &RemovalOutcome) -> anyhow::Result<String> {
    Ok(format!(
        "Removal\nDOTCORE_GC_IMAGE={}\nDOTCORE_GC_NAME={}\nDOTCORE_GC_REASON={}\nDOTCORE_GC_RESULT={}\n",
        record_value(&removal.image_id)?,
        record_value(&removal.reference)?,
        reason(removal.reason),
        record_value(&outcome_text(outcome))?,
    ))
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
    fn records_name_the_removal_and_its_result() {
        let removal = n8n_removal();

        assert_eq!(
            record_block(&removal, &RemovalOutcome::Removed).expect("removed record"),
            "Removal\nDOTCORE_GC_IMAGE=sha256:0123456789abcdef\nDOTCORE_GC_NAME=dotcore-nix/docker-image-n8n:0123\nDOTCORE_GC_REASON=unused name\nDOTCORE_GC_RESULT=removed\n"
        );
        assert!(
            record_block(&removal, &RemovalOutcome::Failed("conflict".to_owned()))
                .expect("failed record")
                .ends_with("DOTCORE_GC_RESULT=failed: conflict\n")
        );
        assert_eq!(short_id(&removal.image_id), "0123456789ab");
    }

    #[test]
    fn nothing_to_remove_needs_no_approval() {
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        assert!(!approve_removals(&mut terminal, &[], false).expect("nothing to approve"));
        assert_eq!(terminal.lines(), ["✔ No Docker image to clean up"]);
    }

    #[test]
    fn removals_without_a_terminal_need_yes() {
        let planned = [n8n_removal()];
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        let error = approve_removals(&mut terminal, &planned, false).expect_err("needs yes");

        assert_eq!(
            error.to_string(),
            "1 Docker image names can be removed; run in a terminal or pass --yes to remove them without confirmation"
        );
        assert!(approve_removals(&mut terminal, &planned, true).expect("yes approves"));
        assert!(terminal.prompts().is_empty());
    }

    #[test]
    fn the_operator_confirms_the_listed_removals() {
        let planned = [n8n_removal()];
        let mut terminal = ScriptedTerminal::interactive([
            ScriptedAnswer::Confirm(true),
            ScriptedAnswer::Confirm(false),
        ]);

        assert!(approve_removals(&mut terminal, &planned, false).expect("confirmed"));
        let declined = approve_removals(&mut terminal, &planned, false).expect_err("declined");

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
