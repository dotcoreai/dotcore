//! The commands that change and inspect a site's membership without copying files between
//! machines: `dotc node add` starts a join on the running manager, `dotc node join` on the new
//! machine reaches it with the one-time token and installs the node's folder,
//! `dotc node remove` takes a worker out of the site, through the running manager or from the
//! stopped site, `dotc node leave` cleans up the removed machine, and `dotc node list` shows the
//! nodes.
//!
//! The token travels through `--token`, a file, or a hidden prompt; it is spent by one join and
//! is cancelled when `node add` exits. Operator prompts and notices use the controlling terminal;
//! results stay on stdout, as plain text or, asked for YAML, one document per command.

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
    output::{OutputFormat, plain_text, present_on},
    terminal::{GridColor, Message, Terminal},
};
use add::{NodeAddArgs, node_add};
pub(crate) use join::RejoinNameMismatch;
use join::{NodeJoinArgs, node_join};
use leave::{NodeLeaveArgs, node_leave};
use list::{NodeListArgs, node_list};
use remove::{NodeRemoveArgs, node_remove};

/// Commands that manage the nodes of a site and this machine's membership in one.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct NodeCommand {
    #[command(subcommand)]
    command: Option<NodeSubcommand>,
}

/// Actions supported by the node command group.
#[derive(Debug, Subcommand)]
enum NodeSubcommand {
    /// Allow one new machine to join the site
    ///
    /// Starts a join on the running site manager and waits for one machine to join through
    /// `dotc node join`.
    Add(NodeAddArgs),

    /// Join this machine to a site with the token `dotc node add` printed
    Join(NodeJoinArgs),

    /// Remove a worker node from the site
    ///
    /// Works through the running manager, or on this stopped site. Run `dotc node leave` on the
    /// removed machine afterwards.
    Remove(NodeRemoveArgs),

    /// Leave the site: stop this node's workloads and delete its node folder
    ///
    /// Runs on the removed machine with its server stopped.
    Leave(NodeLeaveArgs),

    /// List the site's nodes with their addresses and deployments
    ///
    /// Runs on the manager's machine, whether or not the manager is running.
    #[command(alias = "ls")]
    List(NodeListArgs),
}

impl NodeCommand {
    pub(crate) async fn dispatch(
        self,
        paths: &RootPathArgs,
        format: OutputFormat,
    ) -> anyhow::Result<()> {
        match self.command {
            Some(NodeSubcommand::Add(arguments)) => node_add(paths, arguments, format).await,
            Some(NodeSubcommand::Join(arguments)) => node_join(paths, arguments, format).await,
            Some(NodeSubcommand::Remove(arguments)) => node_remove(paths, arguments, format).await,
            Some(NodeSubcommand::Leave(arguments)) => node_leave(paths, arguments, format).await,
            Some(NodeSubcommand::List(arguments)) => node_list(paths, arguments, format),
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

/// Where operator-facing notices go: with a terminal, outcomes are command output and context sits
/// next to the prompts; without one, stderr carries them when the command runs unattended.
struct Notices<'a> {
    terminal: Option<&'a mut dyn Terminal>,
}

impl<'a> Notices<'a> {
    fn new(terminal: Option<&'a mut dyn Terminal>) -> Self {
        Self { terminal }
    }

    /// Shows context the operator can read but an unattended run does not need, next to the
    /// prompts.
    fn info(&mut self, message: Message) -> anyhow::Result<()> {
        if let Some(terminal) = self.terminal.as_deref_mut() {
            terminal.prompt_context(&message)?;
        }
        Ok(())
    }

    /// Reports a positive outcome: as command output on the terminal, else as a plain line on
    /// stdout.
    fn success(&mut self, text: &str) -> anyhow::Result<()> {
        let message = Message::Success(text.to_owned());
        match self.terminal.as_deref_mut() {
            Some(terminal) => present_on(terminal, &[message]),
            None => write_stdout(&plain_text(&[message])),
        }
    }

    /// States a fact the operator still needs after an outcome: as an info line on the terminal,
    /// else as a notice on stderr.
    fn notice(&mut self, text: &str) -> anyhow::Result<()> {
        match self.terminal.as_deref_mut() {
            Some(terminal) => terminal.message(&Message::Info(text.to_owned()))?,
            None => eprintln!("Notice: {text}"),
        }
        Ok(())
    }

    /// Shows something the operator should double-check: next to the prompts on the terminal,
    /// else on stderr.
    fn warn(&mut self, text: &str) -> anyhow::Result<()> {
        match self.terminal.as_deref_mut() {
            Some(terminal) => terminal.prompt_context(&Message::Warning(text.to_owned()))?,
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

/// Reports progress of a long-running command on stderr, where a YAML run keeps what it says
/// besides its one final document.
fn progress(text: &str) {
    eprintln!("{text}");
}

/// Writes plain text to stdout and flushes it so a waiting process sees it promptly.
pub(crate) fn write_stdout(text: &str) -> anyhow::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush())
        .context("write command output")
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
