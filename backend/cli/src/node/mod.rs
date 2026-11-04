//! The commands that change and inspect a site's membership without copying files between
//! machines: `dotc node add` on the stopped manager waits for one new node, `dotc node join` on
//! the new machine reaches it with the one-time token and installs the node's folder,
//! `dotc node remove` takes a worker out of the stopped site, `dotc node leave` cleans up the
//! removed machine, and `dotc node list` shows the nodes.
//!
//! The token travels through `--token`, a file, or a hidden prompt; it is spent by one join and
//! expires when `node add` exits. Operator prompts and notices use the controlling terminal; result records
//! stay on stdout, for example `DOTCORE_NODE_ID=node-0123456789`.

mod add;
mod join;
mod leave;
mod list;
mod remove;

use std::{io::Write, time::Duration};

use anyhow::Context;
use clap::{Args, Subcommand};

use crate::{
    RootPathArgs,
    terminal::{Message, NextStep, Terminal},
};
use add::{NodeAddArgs, node_add};
pub(crate) use join::RejoinNameMismatch;
use join::{NodeJoinArgs, node_join};
use leave::{NodeLeaveArgs, node_leave};
use list::{NodeListArgs, node_list};
use remove::{NodeRemoveArgs, node_remove};

/// Commands that manage site membership: `add` and `remove` on the stopped manager, `join` and
/// `leave` on the node's machine, and `list` on the manager.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct NodeCommand {
    #[command(subcommand)]
    command: Option<NodeSubcommand>,
}

/// Actions supported by the node command group.
#[derive(Debug, Subcommand)]
enum NodeSubcommand {
    /// Wait for one new node to join this stopped site through `dotc node join`; runs on the
    /// stopped manager.
    Add(NodeAddArgs),

    /// Join this machine to a site using the token printed by `dotc node add`.
    Join(NodeJoinArgs),

    /// Remove a worker node from this stopped site; runs on the stopped manager. Run
    /// `dotc node leave` on the removed machine afterwards.
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
            Some(NodeSubcommand::Remove(arguments)) => node_remove(paths, arguments),
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

/// Parses a duration such as `90s`, `15m`, or `2h`; a bare number means seconds.
fn parse_timeout(value: &str) -> Result<Duration, String> {
    let (digits, unit) = match value.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((index, _)) => value.split_at(index),
        None => (value, "s"),
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| "timeout must look like 90s, 15m, or 2h".to_owned())?;
    let seconds = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err("timeout unit must be s, m, or h".to_owned()),
    };
    match amount.checked_mul(seconds) {
        Some(seconds) if seconds > 0 => Ok(Duration::from_secs(seconds)),
        _ => Err("timeout must be positive and not overflow".to_owned()),
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::parse_timeout;

    #[test]
    fn timeouts_accept_units_and_reject_malformed_values() {
        assert_eq!(parse_timeout("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_timeout("15m"), Ok(Duration::from_secs(900)));
        assert_eq!(parse_timeout("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(parse_timeout("45"), Ok(Duration::from_secs(45)));
        for rejected in ["", "m", "0s", "5d", "-1s", "1.5m"] {
            assert!(parse_timeout(rejected).is_err(), "{rejected:?}");
        }
    }
}
