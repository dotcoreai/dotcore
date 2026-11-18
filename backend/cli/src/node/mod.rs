//! The commands that change and inspect a site's membership without copying files between
//! machines: `dotc node add` starts a join on the running manager, `dotc node join` on the new
//! machine reaches it with the one-time token and installs the node's folder,
//! `dotc node remove` takes a worker out of the site, through the running manager or from the
//! stopped site, `dotc node leave` cleans up the removed machine, and `dotc node list` shows the
//! nodes.
//!
//! The token travels through `--token`, a file, or a hidden prompt; it is spent by one join and
//! is cancelled when `node add` exits. Operator prompts and notices use the controlling terminal; result records
//! stay on stdout, for example `DOTCORE_NODE_ID=node-0123456789`.

mod add;
mod join;
mod leave;
mod list;
mod remove;

use std::io::Write;

use anyhow::Context;
use clap::{Args, Subcommand};

use dotcore_site::{
    membership::join::{FingerprintColor, FingerprintPattern},
    node::folder::CertificateFingerprint,
};

use crate::{
    RootPathArgs,
    terminal::{GridColor, Message, NextStep, Terminal},
};
use add::{NodeAddArgs, node_add};
pub(crate) use join::RejoinNameMismatch;
use join::{NodeJoinArgs, node_join};
use leave::{NodeLeaveArgs, node_leave};
use list::{NodeListArgs, node_list};
use remove::{NodeRemoveArgs, node_remove};

/// Commands that manage site membership: `add` and `remove` on the running manager (`remove` also
/// on a stopped one), `join` and `leave` on the node's machine, and `list` on the manager.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct NodeCommand {
    #[command(subcommand)]
    command: Option<NodeSubcommand>,
}

/// Actions supported by the node command group.
#[derive(Debug, Subcommand)]
enum NodeSubcommand {
    /// Start a join on the running site manager and wait for one new node to join through
    /// `dotc node join`.
    Add(NodeAddArgs),

    /// Join this machine to a site using the token printed by `dotc node add`.
    Join(NodeJoinArgs),

    /// Remove a worker node from the site, through the running manager or from this stopped
    /// site. Run `dotc node leave` on the removed machine afterwards.
    Remove(NodeRemoveArgs),

    /// Stop this node's workloads and delete its node folder; runs on the removed machine with
    /// its server stopped.
    Leave(NodeLeaveArgs),

    /// List the nodes of this site with their addresses and deployments; runs on the manager,
    /// which may be running.
    List(NodeListArgs),
}

impl NodeCommand {
    pub(crate) async fn dispatch(self, paths: &RootPathArgs) -> anyhow::Result<()> {
        match self.command {
            Some(NodeSubcommand::Add(arguments)) => node_add(paths, arguments).await,
            Some(NodeSubcommand::Join(arguments)) => node_join(paths, arguments).await,
            Some(NodeSubcommand::Remove(arguments)) => node_remove(paths, arguments).await,
            Some(NodeSubcommand::Leave(arguments)) => node_leave(paths, arguments).await,
            Some(NodeSubcommand::List(arguments)) => node_list(paths, arguments),
            None => anyhow::bail!("node command requires an action"),
        }
    }

    pub(crate) fn argument_contract_error(&self) -> Option<&'static str> {
        match &self.command {
            Some(NodeSubcommand::Join(arguments)) => arguments.argument_contract_error(),
            Some(
                NodeSubcommand::Add(_)
                | NodeSubcommand::Remove(_)
                | NodeSubcommand::Leave(_)
                | NodeSubcommand::List(_),
            )
            | None => None,
        }
    }
}

/// Where operator-facing notices go: the guided terminal when there is one, else stderr, which
/// carries diagnostics when the command runs unattended.
struct Notices<'a> {
    terminal: Option<&'a mut dyn Terminal>,
}

impl<'a> Notices<'a> {
    fn new(terminal: Option<&'a mut dyn Terminal>) -> Self {
        Self { terminal }
    }

    /// Shows context the operator can read but an unattended run does not need.
    fn info(&mut self, message: Message) -> anyhow::Result<()> {
        if let Some(terminal) = self.terminal.as_deref_mut() {
            terminal.message(&message)?;
        }
        Ok(())
    }

    /// Reports a positive outcome on the terminal.
    fn success(&mut self, text: &str) -> anyhow::Result<()> {
        if let Some(terminal) = self.terminal.as_deref_mut() {
            terminal.message(&Message::Success(text.to_owned()))?;
        }
        Ok(())
    }

    /// Tells the operator what to do next: as next steps on the terminal, else as `plain` on
    /// stderr so stdout keeps only records.
    fn instruct(&mut self, steps: Vec<NextStep>, plain: &str) -> anyhow::Result<()> {
        match self.terminal.as_deref_mut() {
            Some(terminal) => terminal.message(&Message::Next(steps))?,
            None => eprintln!("Notice: {plain}"),
        }
        Ok(())
    }

    /// Shows something the operator should double-check.
    fn warn(&mut self, text: &str) -> anyhow::Result<()> {
        match self.terminal.as_deref_mut() {
            Some(terminal) => terminal.message(&Message::Warning(text.to_owned()))?,
            None => eprintln!("Warning: {text}"),
        }
        Ok(())
    }
}

/// Lends an owned terminal to a callee without giving it up.
fn boxed(terminal: &mut Option<Box<dyn Terminal>>) -> Option<&mut dyn Terminal> {
    match terminal {
        Some(terminal) => Some(&mut **terminal),
        None => None,
    }
}

/// Lends an optional terminal again after a callee returned, so one terminal serves every step.
fn lend<'a>(terminal: &'a mut Option<&mut dyn Terminal>) -> Option<&'a mut dyn Terminal> {
    match terminal {
        Some(terminal) => Some(&mut **terminal),
        None => None,
    }
}

/// Writes result records to stdout and flushes them so a waiting process sees them promptly.
pub(crate) fn write_records(records: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(records.as_bytes())
        .and_then(|()| stdout.flush())
        .context("write result records")
}

/// Rejects a record value that could forge an extra record line.
pub(crate) fn record_value(value: &str) -> anyhow::Result<&str> {
    anyhow::ensure!(
        !value.contains(['\r', '\n']),
        "result value must not contain carriage returns or newlines"
    );
    Ok(value)
}

/// The grouped key fingerprint as two facts, the second line under the first, so the operator
/// can compare it with the other machine's by eye.
fn fingerprint_facts(fingerprint: &CertificateFingerprint) -> [(String, String); 2] {
    let [first, second] = fingerprint.grouped_lines();
    [
        ("key fingerprint".to_owned(), first),
        (String::new(), second),
    ]
}

/// The fingerprint pattern of a key as a grid of colored cells, matching the one the other
/// machine shows for the same key.
fn fingerprint_grid(fingerprint: &CertificateFingerprint) -> Message {
    let pattern = FingerprintPattern::of(fingerprint);
    Message::ColorGrid {
        colors: pattern
            .colors
            .iter()
            .map(|color| match color {
                FingerprintColor::Red => GridColor::Red,
                FingerprintColor::Green => GridColor::Green,
                FingerprintColor::Yellow => GridColor::Yellow,
                FingerprintColor::Blue => GridColor::Blue,
                FingerprintColor::Magenta => GridColor::Magenta,
                FingerprintColor::Cyan => GridColor::Cyan,
            })
            .collect(),
        // A slot counts colors from 1, and 0 leaves the cell empty.
        rows: pattern
            .cells
            .iter()
            .map(|row| {
                row.iter()
                    .map(|&slot| usize::from(slot).checked_sub(1))
                    .collect()
            })
            .collect(),
    }
}
