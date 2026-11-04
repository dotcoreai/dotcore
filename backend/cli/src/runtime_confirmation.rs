//! Operator approval before a destructive site command (`dotc site delete`, `dotc site reset`,
//! or its equivalent `dotc site init --replace --force`) removes the runtime resources a stopped
//! site owns.
//!
//! It owns the flags those commands share, reports optional runtimes that could not be
//! inspected, lists the owned resources on the controlling terminal, and asks for confirmation.
//! Without a usable prompt it refuses unless the operator passed `--yes`, and a site that owns
//! nothing needs no approval at all.

use std::{path::Path, sync::Arc};

use anyhow::Context;
use clap::Args;
use dotcore_runtime::{
    SiteRuntimeCleanup, SiteRuntimeCleanups, SiteRuntimeInventory, SkippedSiteRuntimeCleanup,
};
use dotcore_runtime_backends::registry::BackendSiteRuntimeCleanups;
use dotcore_site::reset::transaction::SiteResetSession;

use crate::terminal::{
    ConfirmPrompt, Message, PromptError, Terminal, TerminalCondition, notify_stderr,
};

/// Flags shared by every command that removes a site's runtime resources.
#[derive(Debug, Args)]
pub struct RuntimeRemovalArgs {
    /// Remove the site's runtime resources without asking for confirmation. Required when the
    /// site owns runtime resources and no prompt is possible (no terminal or --non-interactive).
    #[arg(long)]
    pub yes: bool,

    /// Skip Docker and simulated runtime inventory and cleanup, and remove the site anyway, even
    /// when it cannot be identified. Docker resources the site owns may remain. Native workloads
    /// in the site directory are still listed, confirmed, stopped, and removed.
    #[arg(long)]
    pub skip_runtime_cleanup: bool,
}

/// How one command asks for and accepts approval to remove a site's runtime resources.
pub(crate) struct RuntimeRemovalApproval<'a> {
    /// What owns the resources, as the list heading and the refusal name it, for example
    /// `Site at /var/lib/dotcore`.
    pub(crate) subject: String,

    /// Verb phrase named in the refusal, for example `delete`.
    pub(crate) verb: &'a str,

    /// Question asked after the resource list.
    pub(crate) question: &'a str,

    /// Whether the operator passed `--yes`.
    pub(crate) yes: bool,

    /// Whether the operator asked for no prompts even when a terminal is available.
    pub(crate) non_interactive: bool,

    /// Whether the operator passed `--skip-runtime-cleanup`, which removes the site without
    /// listing or cleaning its runtime resources other than native workloads.
    pub(crate) skip_runtime_cleanup: bool,
}

/// Reports skipped runtimes, then obtains approval when the inventory lists resources.
/// A declined prompt returns [`PromptError::Cancelled`], which the CLI maps to exit code 130.
pub(crate) fn approve_runtime_removal(
    terminal: &mut dyn Terminal,
    site_dir: &Path,
    inventory: &SiteRuntimeInventory,
    approval: &RuntimeRemovalApproval<'_>,
) -> anyhow::Result<()> {
    write_skipped_notices(&inventory.skipped)?;
    if approval.skip_runtime_cleanup {
        write_skipped_runtime_cleanup_warning(site_dir)?;
    }
    if inventory.resources.is_empty() || approval.yes {
        return Ok(());
    }
    if approval.non_interactive || terminal.condition() == TerminalCondition::NonInteractive {
        anyhow::bail!(
            "{} owns {} runtime resources; pass --yes to {} without confirmation",
            approval.subject,
            inventory.resources.len(),
            approval.verb
        );
    }

    terminal.message(&Message::Heading(format!(
        "{} owns these resources",
        approval.subject
    )))?;
    terminal.message(&Message::KeyValues(
        inventory
            .resources
            .iter()
            .map(|resource| (resource.runtime.to_owned(), resource.description.clone()))
            .collect(),
    ))?;
    if terminal.confirm(&ConfirmPrompt::new(approval.question, false))? {
        Ok(())
    } else {
        Err(PromptError::Cancelled.into())
    }
}

/// Selects the runtime cleanup the session's existing site configuration binds, naming
/// `--skip-runtime-cleanup` as the way past a binding it cannot classify.
pub(crate) fn site_runtime_cleanup(
    session: &SiteResetSession,
) -> anyhow::Result<Box<dyn SiteRuntimeCleanup>> {
    session.runtime_cleanup().with_context(|| {
        format!(
            "cannot select runtime cleanup for site at {}; repair its execution targets, or pass --skip-runtime-cleanup to remove it without Docker runtime cleanup (native workloads are still removed)",
            session.site_root().display()
        )
    })
}

/// The backends' runtime cleanups that site reset, deletion and node leave select from. A node
/// whose cache root cannot be resolved has no workload caches, so none are removed.
pub(crate) fn site_runtime_cleanups(cache_root: Option<&Path>) -> Arc<dyn SiteRuntimeCleanups> {
    Arc::new(match cache_root {
        Some(cache_root) => BackendSiteRuntimeCleanups::new(cache_root),
        None => BackendSiteRuntimeCleanups::without_cache_root(),
    })
}

/// Context of a native workload cleanup failure of `verb` (`delete` or `reset`): the site is
/// kept, and `--skip-runtime-cleanup` is no way past it.
pub(crate) fn native_cleanup_failure(verb: &str, site_dir: &Path) -> String {
    format!(
        "cannot {verb} the site at {}: its native workloads could not be stopped and removed, so the site directory was kept; resolve the cause below and retry (--skip-runtime-cleanup does not skip native workloads)",
        site_dir.display()
    )
}

/// Warns on stderr that `--skip-runtime-cleanup` may leave runtime resources of an existing site.
fn write_skipped_runtime_cleanup_warning(site_dir: &Path) -> anyhow::Result<()> {
    if !site_dir.join("site.yaml").exists() {
        return Ok(());
    }
    notify_stderr(
        &[Message::Warning(
            "Docker cleanup skipped: Docker resources owned by the site may remain".to_owned(),
        )],
        "Notice: --skip-runtime-cleanup skips Docker runtime cleanup; Docker resources owned by the site may remain",
    );
    Ok(())
}

/// Reports each skipped optional runtime on stderr, which carries CLI diagnostics.
fn write_skipped_notices(skipped: &[SkippedSiteRuntimeCleanup]) -> anyhow::Result<()> {
    for skipped in skipped {
        notify_stderr(
            &[
                Message::Warning(format!(
                    "{} is unreachable, so its cleanup was skipped",
                    skipped.runtime
                )),
                Message::Hint(format!("  {:#}", skipped.reason)),
            ],
            &format!(
                "Notice: skipped {} cleanup because it is unreachable; the site does not bind it, so only resources left by an earlier configuration could remain: {:#}",
                skipped.runtime, skipped.reason
            ),
        );
    }
    Ok(())
}
