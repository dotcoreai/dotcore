//! `dotc dev workload teardown --all`: confirmation and removal of every live development
//! workload recorded under the workspace handle directory.
//!
//! Handles already in the removed state are skipped, so a workspace with nothing left to tear
//! down never loads the catalog or contacts Docker. Every live handle is attempted, a failure
//! on one does not stop the others, and the command fails when any handle could not be removed.

use std::{
    future::Future,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use dotcore::development::{DevelopmentOwnershipHandle, DevelopmentOwnershipState};
use serde_json::{Value, json};

use super::{CommonArgs, Runtime, connect, handle_root, print_success};
use crate::{
    RootPathArgs,
    terminal::{ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition, open_terminal},
};

/// Tears down every live handle after confirmation, printing one JSON success record.
pub(super) async fn teardown_all(
    paths: &RootPathArgs,
    common: &CommonArgs,
    yes: bool,
    non_interactive: bool,
) -> Result<()> {
    let mut terminal = open_terminal();
    let handle_root = handle_root(&paths.master_catalog()?)?;
    let payload = teardown_live_handles(
        &handle_root,
        terminal.as_mut(),
        yes,
        non_interactive,
        |handle| {
            let mut scoped = common.clone();
            scoped.runtime_scope = handle.scope;
            // Each handle is torn down through the runtime that launched its workload.
            scoped.runtime = Some(
                if handle.runtime_backend_id == dotcore_runtime_backends::native::BACKEND_ID {
                    Runtime::Native
                } else {
                    Runtime::Docker
                },
            );
            let handle_path = handle.path;
            async move {
                let facade = connect(paths, &scoped).await?;
                facade
                    .teardown(&handle_path)
                    .await
                    .context("workload teardown failed")
            }
        },
    )
    .await?;
    print_success("teardown", payload);
    Ok(())
}

async fn teardown_live_handles<F, Fut>(
    handle_root: &Path,
    terminal: &mut dyn Terminal,
    yes: bool,
    non_interactive: bool,
    teardown_one: F,
) -> Result<Value>
where
    F: Fn(LiveHandle) -> Fut,
    Fut: Future<Output = Result<DevelopmentOwnershipHandle>>,
{
    let live = live_handles(handle_root)?;
    if !live.is_empty() {
        confirm(terminal, &live, yes, non_interactive)?;
    }

    let mut removed = Vec::new();
    let mut failures = Vec::new();
    for handle in live {
        match teardown_one(handle.clone()).await {
            Ok(result) => removed.push(json!({
                "handle": handle.path,
                "runtime_scope": handle.scope,
                "state": result.state,
            })),
            Err(error) => failures.push(format!(
                "{} (scope {}): {error:#}",
                handle.path.display(),
                handle.scope
            )),
        }
    }
    if !failures.is_empty() {
        bail!(
            "teardown failed for {} development workload handle(s): {}",
            failures.len(),
            failures.join("; ")
        );
    }
    Ok(json!({"handles": removed}))
}

#[derive(Clone, Debug)]
struct LiveHandle {
    path: PathBuf,
    scope: String,
    state: DevelopmentOwnershipState,
    runtime_backend_id: String,
}

/// Lists handle files whose workload is not already removed, scope taken from the file name.
fn live_handles(handle_root: &Path) -> Result<Vec<LiveHandle>> {
    let entries = match std::fs::read_dir(handle_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", handle_root.display()));
        }
    };
    let mut live = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("read {}", handle_root.display()))?
            .path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let scope = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("workload handle name is not UTF-8: {}", path.display()))?;
        let contents = std::fs::read(&path)
            .with_context(|| format!("read workload handle {}", path.display()))?;
        // Only the fields every handle format shares are read, so a handle of another format
        // still reaches teardown, which reports it, instead of failing the whole listing.
        let handle: Value = serde_json::from_slice(&contents)
            .with_context(|| format!("parse workload handle {}", path.display()))?;
        let state: DevelopmentOwnershipState = serde_json::from_value(handle["state"].clone())
            .with_context(|| format!("parse workload handle state {}", path.display()))?;
        if state != DevelopmentOwnershipState::Removed {
            live.push(LiveHandle {
                scope: scope.to_owned(),
                state,
                runtime_backend_id: handle["runtime_backend_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                path,
            });
        }
    }
    live.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(live)
}

fn confirm(
    terminal: &mut dyn Terminal,
    live: &[LiveHandle],
    yes: bool,
    non_interactive: bool,
) -> Result<()> {
    if yes {
        return Ok(());
    }
    if non_interactive || terminal.condition() == TerminalCondition::NonInteractive {
        bail!(
            "{} development workload(s) are live; pass --yes to tear them down without confirmation",
            live.len()
        );
    }
    terminal.message(&Message::Heading(
        "These development workloads will be stopped and removed:".to_owned(),
    ))?;
    terminal.message(&Message::KeyValues(
        live.iter()
            .map(|handle| {
                (
                    handle.scope.clone(),
                    format!("{:?} ({})", handle.state, handle.path.display()),
                )
            })
            .collect(),
    ))?;
    if terminal.confirm(&ConfirmPrompt::new("Tear these workloads down?", false))? {
        Ok(())
    } else {
        Err(PromptError::Cancelled.into())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::terminal::{ScriptedAnswer, ScriptedTerminal};

    #[test]
    fn no_handle_directory_or_only_removed_handles_prompt_and_tear_down_nothing() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        let mut terminal = ScriptedTerminal::interactive([]);
        let calls = Arc::new(Mutex::new(Vec::new()));

        assert_eq!(
            run(&handles, &mut terminal, false, false, &calls).expect("absent"),
            json!({"handles": []})
        );

        write_handle(&handles, "removed-scope", "removed");
        assert_eq!(
            run(&handles, &mut terminal, false, false, &calls).expect("removed"),
            json!({"handles": []})
        );

        assert!(terminal.prompts().is_empty());
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn confirmed_live_handles_are_torn_down_and_removed_handles_are_skipped() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        write_handle(&handles, "local", "running");
        write_handle(&handles, "dirty-scope", "dirty");
        write_handle(&handles, "removed-scope", "removed");
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(true)]);
        let calls = Arc::new(Mutex::new(Vec::new()));

        let payload = run(&handles, &mut terminal, false, false, &calls).expect("teardown");

        assert_eq!(*calls.lock().expect("calls"), ["dirty-scope", "local"]);
        assert_eq!(
            payload["handles"]
                .as_array()
                .expect("handles")
                .iter()
                .map(|handle| handle["runtime_scope"].as_str().expect("scope"))
                .collect::<Vec<_>>(),
            ["dirty-scope", "local"]
        );
        assert_eq!(terminal.messages().len(), 2);
    }

    #[test]
    fn declining_is_a_cancel_and_tears_nothing_down() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        write_handle(&handles, "local", "running");
        let mut terminal = ScriptedTerminal::interactive([ScriptedAnswer::Confirm(false)]);
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = run(&handles, &mut terminal, false, false, &calls).expect_err("declined");

        assert!(matches!(
            error.downcast_ref::<PromptError>(),
            Some(PromptError::Cancelled)
        ));
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn missing_terminal_requires_yes() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        write_handle(&handles, "local", "running");
        let mut terminal = ScriptedTerminal::new(TerminalCondition::NonInteractive, []);
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = run(&handles, &mut terminal, false, false, &calls).expect_err("needs --yes");
        assert_eq!(
            error.to_string(),
            "1 development workload(s) are live; pass --yes to tear them down without confirmation"
        );
        assert!(calls.lock().expect("calls").is_empty());

        run(&handles, &mut terminal, true, false, &calls).expect("--yes");
        assert_eq!(*calls.lock().expect("calls"), ["local"]);
    }

    #[test]
    fn non_interactive_requires_yes_even_with_a_terminal() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        write_handle(&handles, "local", "running");
        let mut terminal = ScriptedTerminal::interactive([]);
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = run(&handles, &mut terminal, false, true, &calls).expect_err("needs --yes");

        assert_eq!(
            error.to_string(),
            "1 development workload(s) are live; pass --yes to tear them down without confirmation"
        );
        assert!(terminal.prompts().is_empty());
        assert!(calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn one_failure_does_not_stop_the_others_and_fails_the_command() {
        let root = crate::test_support::canonical_tempdir().expect("root");
        let handles = root.path().join("development-workloads");
        write_handle(&handles, "a-fails", "dirty");
        write_handle(&handles, "b-works", "running");
        let mut terminal = ScriptedTerminal::interactive([]);
        let calls = Arc::new(Mutex::new(Vec::new()));

        let error = run(&handles, &mut terminal, true, false, &calls).expect_err("one failed");

        assert_eq!(*calls.lock().expect("calls"), ["a-fails", "b-works"]);
        let message = format!("{error:#}");
        assert!(
            message.starts_with("teardown failed for 1 development workload handle(s)"),
            "{message}"
        );
        assert!(message.contains("(scope a-fails): injected teardown failure"));
    }

    fn run(
        handles: &Path,
        terminal: &mut ScriptedTerminal,
        yes: bool,
        non_interactive: bool,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Result<Value> {
        let calls = Arc::clone(calls);
        crate::build_runtime()
            .expect("runtime")
            .block_on(teardown_live_handles(
                handles,
                terminal,
                yes,
                non_interactive,
                move |handle| {
                    let calls = Arc::clone(&calls);
                    let (scope, path) = (handle.scope, handle.path);
                    async move {
                        calls.lock().expect("calls").push(scope.clone());
                        if scope.ends_with("-fails") {
                            bail!("injected teardown failure");
                        }
                        let mut handle: DevelopmentOwnershipHandle =
                            serde_json::from_slice(&std::fs::read(path)?)?;
                        handle.state = DevelopmentOwnershipState::Removed;
                        handle.dirty_reason = None;
                        Ok(handle)
                    }
                },
            ))
    }

    fn write_handle(root: &Path, scope: &str, state: &str) {
        std::fs::create_dir_all(root).expect("handle root");
        let dirty_reason = if state == "dirty" {
            json!("interrupted")
        } else {
            Value::Null
        };
        let running = state == "running";
        std::fs::write(
            root.join(format!("{scope}.json")),
            json!({
                "format_version": 1,
                "state": state,
                "runtime_scope": format!("development-{scope}"),
                "runtime_backend_id": "runtime/docker",
                "owner_id": "dotc-dev-workload",
                "generation": 1,
                "launch_fingerprint": "0".repeat(64),
                "workload_id": running.then_some("workload"),
                "runtime_process_id": running.then_some("container"),
                "process_id": running.then_some(1234),
                "dirty_reason": dirty_reason,
            })
            .to_string(),
        )
        .expect("handle file");
    }
}
