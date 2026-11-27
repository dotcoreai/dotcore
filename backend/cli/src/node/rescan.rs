//! `dotc node rescan`: run on the node whose hardware or runtimes changed, it scans this machine
//! again for the capabilities that node's committed targets offer and replaces its whole target set.
//!
//! A node folder belongs to a joined node: the command proposes the replacement to the site manager
//! and installs what the manager records. A site folder holds a site: the command re-scans that
//! site's own manager node and records the replacement in the site on this machine, which it holds
//! while it plans and writes. Both paths show the change and ask the operator to confirm it, and
//! both report the restart the replacement needs instead of performing it. Nothing is written when
//! the scan finds the whole projection unchanged, and a failure leaves the node with the targets it
//! had.

use std::path::Path;

use anyhow::Context;
use clap::Args;
use dotcore::model::engine::declaration_validator;
use dotcore_domain::diagnostic::diagnostic_chain;
use dotcore_proto::dotcore::site::v1::ExecutionTarget;
use dotcore_site::{
    SITE_FILE,
    initial::{
        probe::InitialSiteOffer, request::InitialSiteRequest, validation::SiteInputBundleValidator,
    },
    membership::{
        ManagerNodeRescan, ManagerNodeRescanError, ManagerRescanOutcome, NodeRescan,
        NodeRescanError, NodeRescanOutcome, RescanClassification, RescanCommitError, RescanError,
        RescanInstallError, RescanReplacement, RescanScan,
    },
};

use super::{Notices, boxed, lend, record_value, write_records};
use crate::{
    RootPathArgs,
    site::system_initial_site_prober,
    terminal::{
        ConfirmPrompt, Message, NextStep, PromptError, Terminal, TerminalCondition, open_terminal,
    },
};

/// Arguments of `dotc node rescan`.
#[derive(Debug, Args)]
pub struct NodeRescanArgs {
    /// Replace the targets without showing and confirming them first.
    #[arg(long)]
    pub yes: bool,
}

/// Shown after a joined node sends its proposal, while the manager's operator decides.
const APPROVAL_NOTICE: &str = "Waiting for approval on the manager...";

/// Re-scans this machine and replaces the target set of the node this folder belongs to: a joined
/// node through its site manager, the manager node of a site through the site itself.
pub(crate) async fn node_rescan(
    paths: &RootPathArgs,
    arguments: NodeRescanArgs,
) -> anyhow::Result<()> {
    let site_dir = paths.site_dir()?;
    let terminal = open_terminal();
    if !arguments.yes && terminal.condition() == TerminalCondition::NonInteractive {
        anyhow::bail!(
            "re-scanning a node's targets shows the replacement and needs a terminal; run in a terminal or pass --yes to replace them without confirmation"
        );
    }
    let mut terminal = Some(terminal);

    // A site folder holds the site itself, so its own manager node is re-scanned here; a node
    // folder belongs to a joined node, which proposes its replacement to its site manager.
    if site_dir.join(SITE_FILE).is_file() {
        return rescan_manager_node(&site_dir, arguments.yes, boxed(&mut terminal)).await;
    }
    rescan_joined_node(&site_dir, arguments.yes, boxed(&mut terminal)).await
}

/// Re-scans a joined node and installs what its site manager records.
async fn rescan_joined_node(
    site_dir: &Path,
    unattended: bool,
    mut terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    // Nothing the proposal carries may come from a copy read earlier, so the folder is re-read and
    // re-validated here, under the node's custody lock, which refuses a running node server.
    let node = NodeRescan::open(site_dir).map_err(|error| {
        anyhow::Error::new(error)
            .context(format!("cannot re-scan the node at {}", site_dir.display()))
    })?;
    let offer = probe_machine(&node.scan_request()).await;
    let scan = node
        .scan(&offer.report)
        .context("scan this machine's replacement targets")?;
    let classification = RescanClassification::classify(node.committed_targets(), &scan.targets);

    if !unattended {
        review(
            &Review::joined_node(&node),
            &classification,
            lend(&mut terminal),
        )?;
    }
    warn_unavailable(&scan, &offer, lend(&mut terminal))?;

    let site_id = node.site_id().to_owned();
    let node_id = node.node_id().to_owned();
    let outcome = node
        .propose(scan.targets)
        .await
        .map_err(|error| joined_node_failure(error, &node_id))?;
    let outcome_report = match outcome {
        NodeRescanOutcome::Installed { restart_required } => Report::installed(restart_required),
        NodeRescanOutcome::Unchanged => Report::unchanged(),
    };
    report(
        &outcome_report,
        &site_id,
        &node_id,
        lend(&mut terminal),
        unattended,
    )
}

/// Re-scans the manager node of the site this folder holds and records what the operator confirms.
async fn rescan_manager_node(
    site_dir: &Path,
    unattended: bool,
    mut terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    // The site is held from planning through the write, so a running site manager, or another site
    // command writing the site, is refused before anything is read.
    let rescan = ManagerNodeRescan::open(
        site_dir,
        &SiteInputBundleValidator::new(
            declaration_validator(),
            dotcore::model::site_declaration_rules(),
        ),
    )
    .map_err(|error| {
        anyhow::Error::new(error)
            .context(format!("cannot re-scan the site at {}", site_dir.display()))
    })?;
    let offer = probe_machine(&rescan.scan_request()).await;
    let scan = rescan
        .scan(&offer.report)
        .context("scan this machine's replacement targets")?;
    let site_id = rescan.site_id().to_owned();
    let node_id = rescan.node_id().to_owned();

    // The replacement is derived and validated against the site before the operator is asked, so a
    // deployment guard refusal or an invalid replacement never reaches a confirmation prompt.
    let plan: RescanReplacement = rescan
        .plan(scan.targets.clone())
        .map_err(|error| manager_node_failure(error, &node_id))?;
    if !unattended {
        review(
            &Review::manager_node(&rescan),
            plan.classification(),
            lend(&mut terminal),
        )?;
    }
    warn_unavailable(&scan, &offer, lend(&mut terminal))?;

    let outcome = rescan
        .commit(&plan)
        .map_err(|error| manager_node_failure(error, &node_id))?;
    let outcome_report = match outcome {
        ManagerRescanOutcome::Recorded => Report::recorded(),
        ManagerRescanOutcome::Unchanged => Report::unchanged(),
    };
    report(
        &outcome_report,
        &site_id,
        &node_id,
        lend(&mut terminal),
        unattended,
    )
}

/// What a re-scan reports, and whether the node still owes the restart it serves under.
struct Report {
    summary: &'static str,
    restart_required: bool,
}

impl Report {
    /// The site recorded the replacement and this joined node installed it.
    fn installed(restart_required: bool) -> Self {
        Self {
            summary: "The site recorded the replacement and this node installed it.",
            restart_required,
        }
    }

    /// The site recorded the replacement in its own declaration, on this machine.
    fn recorded() -> Self {
        Self {
            summary: "The site recorded the replacement in its own declaration.",
            restart_required: true,
        }
    }

    /// The whole projection needs no bytes, on either path.
    fn unchanged() -> Self {
        Self {
            summary: "The whole projection is unchanged: nothing was written and no restart is needed.",
            restart_required: false,
        }
    }
}

/// Probes this machine for every capability a node can offer; one the machine lacks or can't use is
/// left out of the report rather than failing the re-scan.
async fn probe_machine(request: &InitialSiteRequest) -> InitialSiteOffer {
    system_initial_site_prober().probe_offered(request).await
}

/// What the operator reviews before a replacement, whichever path re-scans the node.
struct Review {
    /// Names the node whose targets change and the site it belongs to.
    heading: String,

    /// Label and path of the folder the committed targets were read from.
    folder: (&'static str, String),

    /// Shown when the scan found the whole projection unchanged.
    unchanged: &'static str,

    /// Asked before the replacement goes ahead.
    confirm: &'static str,

    /// Shown after the confirmation while a joined node waits for its manager to decide; a manager
    /// node records its own replacement, so it waits for nothing.
    notice: Option<&'static str>,
}

impl Review {
    fn joined_node(node: &NodeRescan) -> Self {
        Self {
            heading: format!(
                "Node {} of site {} re-scans its targets",
                node.node_id(),
                node.site_id()
            ),
            folder: ("node folder", node.node_root().display().to_string()),
            unchanged: "The scan found this node's targets unchanged; the site manager confirms \
                        that, writes nothing and no restart is needed.",
            confirm: "Propose this replacement to the site manager?",
            notice: Some(APPROVAL_NOTICE),
        }
    }

    fn manager_node(rescan: &ManagerNodeRescan) -> Self {
        Self {
            heading: format!(
                "Manager node {} of site {} re-scans its targets",
                rescan.node_id(),
                rescan.site_id()
            ),
            folder: ("site folder", rescan.site_root().display().to_string()),
            unchanged: "The scan found this node's targets unchanged: nothing is written and no \
                        restart is needed.",
            confirm: "Replace this node's targets in the site?",
            notice: None,
        }
    }
}

/// Shows what the node would replace, and asks the operator to confirm it.
fn review(
    review: &Review,
    classification: &RescanClassification,
    mut terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    {
        let mut notices = Notices::new(lend(&mut terminal));
        notices.info(Message::Heading(review.heading.clone()))?;
        notices.info(Message::KeyValues(vec![(
            review.folder.0.to_owned(),
            review.folder.1.clone(),
        )]))?;
        notices.info(Message::Heading("Proposed targets".to_owned()))?;
        notices.info(Message::KeyValues(vec![
            (
                "unchanged".to_owned(),
                target_names(&classification.unchanged),
            ),
            ("changed".to_owned(), target_names(&classification.changed)),
            ("added".to_owned(), target_names(&classification.added)),
            ("removed".to_owned(), target_names(&classification.removed)),
        ]))?;
        if classification.changed.is_empty()
            && classification.added.is_empty()
            && classification.removed.is_empty()
        {
            notices.info(Message::Info(review.unchanged.to_owned()))?;
        }
    }
    let Some(terminal) = terminal else {
        return Ok(());
    };
    if !terminal.confirm(&ConfirmPrompt::new(review.confirm, false))? {
        return Err(PromptError::Cancelled.into());
    }
    if let Some(notice) = review.notice {
        Notices::new(Some(terminal)).info(Message::Info(notice.to_owned()))?;
    }
    Ok(())
}

/// Reports each capability this machine no longer offers, whose targets stay as declared, and each
/// one present here but unusable, with why, so no target is added or refreshed for it.
fn warn_unavailable(
    scan: &RescanScan,
    offer: &InitialSiteOffer,
    mut terminal: Option<&mut dyn Terminal>,
) -> anyhow::Result<()> {
    let mut notices = Notices::new(lend(&mut terminal));
    for error in &offer.unusable {
        notices.warn(&format!(
            "This machine has a capability it can't use, so the re-scan offers no target for it: {}",
            diagnostic_chain(error)
        ))?;
    }
    for (capability, targets) in &scan.unavailable {
        notices.warn(&format!(
            "This machine no longer offers {}; its {} {} kept as declared and report unavailable: {}",
            capability.label(),
            targets.len(),
            if targets.len() == 1 {
                "target stays"
            } else {
                "targets stay"
            },
            target_names(targets),
        ))?;
    }
    Ok(())
}

/// Reports what the re-scan left, on the terminal when interactive, else as records. The restart is
/// reported only for a replacement the site recorded durably.
fn report(
    report: &Report,
    site_id: &str,
    node_id: &str,
    mut terminal: Option<&mut dyn Terminal>,
    unattended: bool,
) -> anyhow::Result<()> {
    if unattended {
        return write_records(&format!(
            "Node re-scanned\nDOTCORE_SITE_ID={}\nDOTCORE_NODE_ID={}\nDOTCORE_RESTART_REQUIRED={}\n",
            record_value(site_id)?,
            record_value(node_id)?,
            report.restart_required,
        ));
    }
    let mut notices = Notices::new(lend(&mut terminal));
    notices.success(report.summary)?;
    if report.restart_required {
        notices.instruct(
            vec![NextStep::new(
                "Start this node again to serve under its replacement targets; until it restarts it \
                 does not serve under them.",
            )
            .with_command("dotcore")],
            "Start the node again to serve under its replacement targets.",
        )?;
    }
    Ok(())
}

/// Names the targets of one classification group, in the order the scan produced them.
fn target_names(targets: &[ExecutionTarget]) -> String {
    if targets.is_empty() {
        return "none".to_owned();
    }
    targets
        .iter()
        .map(|target| target.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Adds what a failed joined-node re-scan left behind, so the operator knows whether the site
/// recorded anything and whether this node installed it.
fn joined_node_failure(error: NodeRescanError, node_id: &str) -> anyhow::Error {
    let context = match &error {
        NodeRescanError::NotDeclared { .. } => format!(
            "node {node_id} is no longer declared in this site; its targets were not replaced"
        ),
        NodeRescanError::Refused { .. } => format!(
            "the site manager refused the replacement for {node_id}; nothing was written on either side"
        ),
        NodeRescanError::OutcomeUnknown { .. } => format!(
            "the site may or may not have recorded the replacement for {node_id}; this node installed nothing, so run the re-scan again once the cause below is resolved"
        ),
        NodeRescanError::Unreachable { .. } => format!(
            "the site manager was not reached, so no replacement was sent for {node_id}; this node keeps the targets it had"
        ),
        NodeRescanError::Install(RescanInstallError::WriteUncertain { node_root, .. }) => format!(
            "the site recorded the replacement for {node_id}, but installing it in {} ended without a known outcome; do not start this node until the re-scan is run again and reads the site's declaration",
            node_root.display()
        ),
        NodeRescanError::Install(_) => format!(
            "the site recorded the replacement for {node_id}, but this node did not install it, so it keeps the targets it had; run the re-scan again once the cause below is resolved"
        ),
        _ => format!("the re-scan of {node_id} stopped before this node changed its targets"),
    };
    anyhow::Error::new(error).context(context)
}

/// Adds what a failed manager-node re-scan left behind, so the operator knows whether the site
/// recorded the replacement or kept the declaration it had.
fn manager_node_failure(error: ManagerNodeRescanError, node_id: &str) -> anyhow::Error {
    let context = match &error {
        ManagerNodeRescanError::Refused(RescanError::DeploymentGuard { .. }) => format!(
            "the replacement for {node_id} was refused because it would change or remove targets \
             that carry deployments; the site kept the targets it had",
        ),
        ManagerNodeRescanError::Refused(_) => {
            format!("the replacement for {node_id} was refused; the site kept the targets it had")
        }
        ManagerNodeRescanError::Commit(RescanCommitError::OutcomeUncertain(_)) => format!(
            "the site may or may not have recorded the replacement for {node_id}; nothing else \
             changed, so run the re-scan again once the cause below is resolved",
        ),
        ManagerNodeRescanError::Commit(_) => format!(
            "the site could not record the replacement for {node_id}; it kept the targets it had",
        ),
        _ => format!("the re-scan of {node_id} stopped before the site changed its targets"),
    };
    anyhow::Error::new(error).context(context)
}

#[cfg(test)]
mod tests {
    use super::{ManagerNodeRescanError, Report, RescanCommitError, manager_node_failure, report};
    use crate::terminal::{Message, ScriptedTerminal};

    /// A committed replacement is reported as installed together with the restart the node still
    /// owes, which the command states instead of performing itself.
    #[test]
    fn an_installed_replacement_is_reported_with_the_restart_it_owes() {
        let mut terminal = ScriptedTerminal::interactive([]);

        report(
            &Report::installed(true),
            "site-one",
            "node-one",
            Some(&mut terminal),
            false,
        )
        .expect("report the installed replacement");

        let messages = terminal.messages();
        let summary = messages
            .iter()
            .find_map(|message| match message {
                Message::Success(text) => Some(text.clone()),
                _ => None,
            })
            .expect("the success summary");
        assert!(summary.contains("installed it"), "summary: {summary}");
        let steps = messages
            .iter()
            .find_map(|message| match message {
                Message::Next(steps) => Some(steps.clone()),
                _ => None,
            })
            .expect("the restart instruction");
        assert_eq!(steps.len(), 1);
        assert!(steps[0].text().contains("Start this node again"));
        assert_eq!(steps[0].command(), Some("dotcore"));
    }

    /// A replacement the manager node recorded in its own site is reported with the restart its own
    /// server owes, which the command states instead of performing itself.
    #[test]
    fn a_recorded_manager_replacement_is_reported_with_the_restart_it_owes() {
        let mut terminal = ScriptedTerminal::interactive([]);

        report(
            &Report::recorded(),
            "site-one",
            "node-one",
            Some(&mut terminal),
            false,
        )
        .expect("report the recorded replacement");

        let messages = terminal.messages();
        let summary = messages
            .iter()
            .find_map(|message| match message {
                Message::Success(text) => Some(text.clone()),
                _ => None,
            })
            .expect("the success summary");
        assert!(
            summary.contains("its own declaration"),
            "summary: {summary}"
        );
        let steps = messages
            .iter()
            .find_map(|message| match message {
                Message::Next(steps) => Some(steps.clone()),
                _ => None,
            })
            .expect("the restart instruction");
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].command(), Some("dotcore"));
    }

    /// A whole projection needing no bytes is reported as unchanged, with no restart asked for.
    #[test]
    fn an_unchanged_projection_is_reported_without_a_restart() {
        let mut terminal = ScriptedTerminal::interactive([]);

        report(
            &Report::unchanged(),
            "site-one",
            "node-one",
            Some(&mut terminal),
            false,
        )
        .expect("report the unchanged projection");

        assert!(terminal.messages().iter().any(|message| matches!(
            message,
            Message::Success(text) if text.contains("unchanged")
        )));
        assert!(
            !terminal
                .messages()
                .iter()
                .any(|message| matches!(message, Message::Next(_))),
            "an unchanged projection needs no next step"
        );
    }

    /// A write the site could not complete is reported as leaving the targets it had, so the
    /// operator knows the site was not changed and can fix the cause and run the command again.
    #[test]
    fn a_failed_manager_write_reports_the_targets_the_site_kept() {
        let error = manager_node_failure(
            ManagerNodeRescanError::Commit(RescanCommitError::Failed(anyhow::anyhow!(
                "the site file could not be written"
            ))),
            "node-one",
        );

        let reported = format!("{error:#}");
        assert!(
            reported.contains("the site could not record the replacement for node-one"),
            "{reported}"
        );
        assert!(
            reported.contains("it kept the targets it had"),
            "{reported}"
        );
    }
}
