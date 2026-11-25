//! The shared workload storage step of `dotc gc`: lists the storage that expired on this machine,
//! asks for confirmation, removes it and reports each result.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use dotcore_domain::real_time;
use dotcore_runtime::WorkloadStorageKind;
use dotcore_runtime_backends::docker::{
    DockerMountedFolders, StorageArea, StorageCleanup, StorageRemoval, StorageRemovalOutcome,
};
use serde::Serialize;

use super::{Approval, approve};
use crate::{
    RootPathArgs,
    output::{LazyTerminal, OutputFormat, present_on},
    terminal::{Message, Terminal},
};

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// What happened to one shared workload storage folder.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct StorageResult {
    kind: &'static str,
    area: String,
    namespace: String,
    key: String,
    result: &'static str,
}

impl StorageResult {
    fn new(removal: &StorageRemoval, outcome: &StorageRemovalOutcome) -> Self {
        Self {
            kind: kind(removal.kind),
            area: removal.area.clone(),
            namespace: removal.namespace.clone(),
            key: removal.key.clone(),
            result: match outcome {
                StorageRemovalOutcome::Removed => "removed",
                StorageRemovalOutcome::InUse => "inUse",
                StorageRemovalOutcome::UsedSincePlanning => "usedSincePlanning",
                StorageRemovalOutcome::Failed(_) => "failed",
            },
        }
    }
}

/// Removes the shared workload storage that no workload used for more than `unused_days` days,
/// in every site's caches on this machine, the development data and the site folder's data, and
/// returns what happened to each folder, or none when the operator declined or there was nothing
/// to remove.
pub(super) async fn clean(
    paths: &RootPathArgs,
    unused_days: u32,
    yes: bool,
    format: OutputFormat,
) -> anyhow::Result<Vec<StorageResult>> {
    let site_root = paths.site_dir()?;
    let areas = StorageArea::on_machine(&paths.cache_root()?, Some(&site_root))
        .context("Workload storage listing failed")?;
    let mounts = DockerMountedFolders::connect()
        .await
        .context("cannot reach the Docker daemon")?;
    let cleanup = StorageCleanup::new(areas, DAY * unused_days, Arc::new(mounts));
    let now = real_time::wall_now();
    let planned = cleanup
        .plan(now)
        .await
        .context("Workload storage cleanup planning failed")?;
    let mut terminal = LazyTerminal::new();
    if !approve_storage(&mut terminal, &planned, now, yes, format)? {
        return Ok(Vec::new());
    }

    let results = cleanup.apply(planned, real_time::wall_now()).await;
    if format == OutputFormat::Human {
        show_results(&mut terminal, &results, now)?;
    }
    let failed = results
        .iter()
        .filter(|(_, outcome)| matches!(outcome, StorageRemovalOutcome::Failed(_)))
        .count();
    anyhow::ensure!(failed == 0, "{failed} workload storage removals failed");
    Ok(results
        .iter()
        .map(|(removal, outcome)| StorageResult::new(removal, outcome))
        .collect())
}

/// Decides whether the planned storage removals go ahead, as [`approve`] does.
fn approve_storage(
    terminal: &mut dyn Terminal,
    planned: &[StorageRemoval],
    now: SystemTime,
    yes: bool,
    format: OutputFormat,
) -> anyhow::Result<bool> {
    approve(
        terminal,
        Approval {
            nothing: "No unused workload storage to clean up",
            noun: "unused workload storage folders",
            header: &HEADER,
            rows: planned
                .iter()
                .map(|removal| removal_row(removal, now))
                .collect(),
        },
        yes,
        format,
    )
}

const HEADER: [&str; 5] = ["KIND", "AREA", "NAMESPACE", "KEY", "UNUSED"];

fn show_results(
    terminal: &mut dyn Terminal,
    results: &[(StorageRemoval, StorageRemovalOutcome)],
    now: SystemTime,
) -> anyhow::Result<()> {
    present_on(
        terminal,
        &[Message::Table {
            header: HEADER
                .iter()
                .chain(&["RESULT"])
                .map(|&title| title.to_owned())
                .collect(),
            rows: results
                .iter()
                .map(|(removal, outcome)| {
                    let mut row = removal_row(removal, now);
                    row.push(outcome_text(outcome));
                    row
                })
                .collect(),
        }],
    )?;
    let removed = results
        .iter()
        .filter(|(_, outcome)| outcome == &StorageRemovalOutcome::Removed)
        .count();
    present_on(
        terminal,
        &[Message::Success(format!(
            "Removed {removed} of {} unused workload storage folders",
            results.len()
        ))],
    )?;
    Ok(())
}

fn removal_row(removal: &StorageRemoval, now: SystemTime) -> Vec<String> {
    vec![
        kind(removal.kind).to_owned(),
        removal.area.clone(),
        removal.namespace.clone(),
        removal.key.clone(),
        unused_text(removal, now),
    ]
}

fn kind(kind: WorkloadStorageKind) -> &'static str {
    match kind {
        WorkloadStorageKind::Cache => "cache",
        WorkloadStorageKind::Data => "data",
    }
}

fn unused_text(removal: &StorageRemoval, now: SystemTime) -> String {
    let days = now
        .duration_since(removal.last_used)
        .unwrap_or_default()
        .as_secs()
        / DAY.as_secs();
    format!("{days} days")
}

fn outcome_text(outcome: &StorageRemovalOutcome) -> String {
    match outcome {
        StorageRemovalOutcome::Removed => "removed".to_owned(),
        StorageRemovalOutcome::InUse => "in use".to_owned(),
        StorageRemovalOutcome::UsedSincePlanning => "used since listed".to_owned(),
        StorageRemovalOutcome::Failed(detail) => format!("failed: {detail}"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::terminal::{
        ConfirmPrompt, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal, TerminalCondition,
    };

    #[test]
    fn the_operator_confirms_the_listed_storage() {
        let now = SystemTime::UNIX_EPOCH + DAY * 30;
        let planned = [engine_cache(now - DAY * 9)];
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        assert!(
            approve_storage(&mut terminal, &planned, now, false, OutputFormat::Human)
                .expect("confirmed")
        );

        assert_eq!(
            terminal.messages()[0],
            Message::Table {
                header: HEADER.map(str::to_owned).to_vec(),
                rows: vec![vec![
                    "cache".to_owned(),
                    "site-a".to_owned(),
                    "engine-artifact".to_owned(),
                    "sha256-0123".to_owned(),
                    "9 days".to_owned(),
                ]],
            }
        );
        assert_eq!(
            terminal.prompts(),
            [ScriptedPrompt::Confirm(ConfirmPrompt::new(
                "Remove these 1 unused workload storage folders?",
                false,
            ))]
        );
    }

    #[test]
    fn nothing_to_remove_and_no_terminal_behave_as_for_images() {
        let now = SystemTime::UNIX_EPOCH + DAY * 30;
        let mut shown = ScriptedTerminal::interactive([]);

        assert!(
            !approve_storage(&mut shown, &[], now, false, OutputFormat::Human)
                .expect("nothing to approve")
        );
        assert_eq!(shown.lines(), ["✔ No unused workload storage to clean up"]);
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);
        let error = approve_storage(
            &mut terminal,
            &[engine_cache(now)],
            now,
            false,
            OutputFormat::Human,
        )
        .expect_err("needs yes");
        assert_eq!(
            error.to_string(),
            "1 unused workload storage folders can be removed; run in a terminal or pass --yes to remove them without confirmation"
        );
    }

    #[test]
    fn the_document_names_the_storage_and_its_result() {
        let removal = engine_cache(SystemTime::UNIX_EPOCH);

        assert_eq!(
            serde_json::to_value(StorageResult::new(
                &removal,
                &StorageRemovalOutcome::UsedSincePlanning
            ))
            .expect("document"),
            serde_json::json!({
                "kind": "cache",
                "area": "site-a",
                "namespace": "engine-artifact",
                "key": "sha256-0123",
                "result": "usedSincePlanning",
            })
        );
    }

    fn engine_cache(last_used: SystemTime) -> StorageRemoval {
        StorageRemoval {
            kind: WorkloadStorageKind::Cache,
            area: "site-a".to_owned(),
            namespace: "engine-artifact".to_owned(),
            key: "sha256-0123".to_owned(),
            path: PathBuf::from("/cache/workload-cache/v2/site-a/engine-artifact/sha256-0123"),
            last_used,
        }
    }
}
