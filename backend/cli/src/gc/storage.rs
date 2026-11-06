//! The shared workload storage step of `dotc gc`: lists the storage that expired on this machine,
//! asks for confirmation, removes it and reports each result.

use std::{
    sync::Arc,
    time::{Duration, SystemTime},
};

use anyhow::Context as _;
use dotcore_runtime::WorkloadStorageKind;
use dotcore_runtime_backends::docker::{
    DockerMountedFolders, StorageArea, StorageCleanup, StorageRemoval, StorageRemovalOutcome,
};

use super::{Approval, approve};
use crate::{
    RootPathArgs,
    node::{record_value, write_records},
    terminal::{
        Message, Terminal, TerminalCondition, controlling_terminal_condition, open_terminal,
    },
};

// Not a behavior duration: a unit for age thresholds an operator states in days.
const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// Removes the shared workload storage that no workload used for more than `unused_days` days,
/// in every site's caches on this machine, the development data and the site folder's data.
pub(super) async fn clean(paths: &RootPathArgs, unused_days: u32, yes: bool) -> anyhow::Result<()> {
    let site_root = paths.site_dir()?;
    let areas = StorageArea::on_machine(&paths.cache_root()?, Some(&site_root))
        .context("Workload storage listing failed")?;
    let mounts = DockerMountedFolders::connect()
        .await
        .context("cannot reach the Docker daemon")?;
    let cleanup = StorageCleanup::new(areas, DAY * unused_days, Arc::new(mounts));
    let now = SystemTime::now();
    let planned = cleanup
        .plan(now)
        .await
        .context("Workload storage cleanup planning failed")?;
    let mut terminal = open_terminal();
    if !approve_storage(terminal.as_mut(), &planned, now, yes)? {
        return Ok(());
    }

    let results = cleanup.apply(planned, SystemTime::now()).await;
    if controlling_terminal_condition() == TerminalCondition::Interactive {
        show_results(terminal.as_mut(), &results, now)?;
    } else {
        let blocks = results
            .iter()
            .map(|(removal, outcome)| record_block(removal, outcome))
            .collect::<anyhow::Result<Vec<_>>>()?;
        write_records(&blocks.join("\n"))?;
    }
    let failed = results
        .iter()
        .filter(|(_, outcome)| matches!(outcome, StorageRemovalOutcome::Failed(_)))
        .count();
    anyhow::ensure!(failed == 0, "{failed} workload storage removals failed");
    Ok(())
}

/// Decides whether the planned storage removals go ahead, as [`approve`] does.
fn approve_storage(
    terminal: &mut dyn Terminal,
    planned: &[StorageRemoval],
    now: SystemTime,
    yes: bool,
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
    )
}

const HEADER: [&str; 5] = ["KIND", "AREA", "NAMESPACE", "KEY", "UNUSED"];

fn show_results(
    terminal: &mut dyn Terminal,
    results: &[(StorageRemoval, StorageRemovalOutcome)],
    now: SystemTime,
) -> anyhow::Result<()> {
    terminal.message(&Message::Table {
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
    })?;
    let removed = results
        .iter()
        .filter(|(_, outcome)| outcome == &StorageRemovalOutcome::Removed)
        .count();
    terminal.message(&Message::Success(format!(
        "Removed {removed} of {} unused workload storage folders",
        results.len()
    )))?;
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

fn record_block(
    removal: &StorageRemoval,
    outcome: &StorageRemovalOutcome,
) -> anyhow::Result<String> {
    Ok(format!(
        "StorageRemoval\nDOTCORE_GC_STORAGE_KIND={}\nDOTCORE_GC_STORAGE_AREA={}\nDOTCORE_GC_STORAGE_NAMESPACE={}\nDOTCORE_GC_STORAGE_KEY={}\nDOTCORE_GC_STORAGE_RESULT={}\n",
        kind(removal.kind),
        record_value(&removal.area)?,
        record_value(&removal.namespace)?,
        record_value(&removal.key)?,
        record_value(&outcome_text(outcome))?,
    ))
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
    use crate::terminal::{ConfirmPrompt, ScriptedAnswer, ScriptedPrompt, ScriptedTerminal};

    #[test]
    fn the_operator_confirms_the_listed_storage() {
        let now = SystemTime::UNIX_EPOCH + DAY * 30;
        let planned = [engine_cache(now - DAY * 9)];
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);

        assert!(approve_storage(&mut terminal, &planned, now, false).expect("confirmed"));

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
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);

        assert!(!approve_storage(&mut terminal, &[], now, false).expect("nothing to approve"));
        assert_eq!(
            terminal.lines(),
            ["✔ No unused workload storage to clean up"]
        );
        let error = approve_storage(&mut terminal, &[engine_cache(now)], now, false)
            .expect_err("needs yes");
        assert_eq!(
            error.to_string(),
            "1 unused workload storage folders can be removed; run in a terminal or pass --yes to remove them without confirmation"
        );
    }

    #[test]
    fn records_name_the_storage_and_its_result() {
        let removal = engine_cache(SystemTime::UNIX_EPOCH);

        assert_eq!(
            record_block(&removal, &StorageRemovalOutcome::InUse).expect("record"),
            "StorageRemoval\nDOTCORE_GC_STORAGE_KIND=cache\nDOTCORE_GC_STORAGE_AREA=site-a\nDOTCORE_GC_STORAGE_NAMESPACE=engine-artifact\nDOTCORE_GC_STORAGE_KEY=sha256-0123\nDOTCORE_GC_STORAGE_RESULT=in use\n"
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
