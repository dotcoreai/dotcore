//! The agent skill printed by `dotc --skill`.
//!
//! It renders a Markdown skill document: frontmatter naming the skill, a static guide to the
//! conventions every command shares, and the list of every command generated from the command
//! tree, so the list always matches the binary an agent runs.

use anyhow::Result;

use crate::output;

const GUIDE: &str = include_str!("guide.md");

const DESCRIPTION: &str = "Manage a Dotcore site from the terminal. Inspect and control model and \
    application deployments, nodes and sites, and check this machine's hardware. Use when the \
    user asks to deploy, list, pause, resume, inspect or remove Dotcore models or apps, or to \
    run or debug Dotcore itself.";

const COMPATIBILITY: &str =
    "Requires dotc on PATH and, for site commands, a running Dotcore manager.";

/// Prints the skill for `root` on stdout, as raw Markdown whatever the terminal.
pub(crate) fn print(root: &clap::Command) -> Result<()> {
    output::write_text(&render(root))
}

/// Renders the complete skill document for the command tree `root`.
fn render(root: &clap::Command) -> String {
    let mut root = root.clone();
    root.build();
    format!(
        "---\nname: dotc\ndescription: {DESCRIPTION}\ncompatibility: {COMPATIBILITY}\nmetadata:\n  version: \"{}\"\n---\n{GUIDE}\n## Commands\n\n{}",
        env!("CARGO_PKG_VERSION"),
        render_commands(&root)
    )
}

/// Renders the visible leaf commands grouped by top-level command, one `dotc <path> <arguments>`
/// line each with its one-line description, padded to the widest entry.
fn render_commands(root: &clap::Command) -> String {
    let mut groups: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for group in visible_subcommands(root) {
        let mut entries = Vec::new();
        collect_leaves(group, "dotc", &mut entries);
        groups.push((group.get_name().to_string(), entries));
    }
    let width = groups
        .iter()
        .flat_map(|(_, entries)| entries)
        .map(|(usage, _)| usage.len())
        .max()
        .unwrap_or(0);

    let mut text = String::new();
    for (name, entries) in &groups {
        text.push_str(&format!("--- {name} ---\n"));
        for (usage, about) in entries {
            text.push_str(&format!("{usage:<width$}  # {about}\n"));
        }
        text.push('\n');
    }
    text.push_str("Run `dotc <command> --help` for flags.\n");
    text
}

/// Adds `command` or, for a group, its leaves in declaration order.
fn collect_leaves(command: &clap::Command, path: &str, entries: &mut Vec<(String, String)>) {
    let path = format!("{path} {}", command.get_name());
    let mut children = visible_subcommands(command).peekable();
    if children.peek().is_none() {
        entries.push((usage(command, &path), about(command)));
        return;
    }
    for child in children {
        collect_leaves(child, &path, entries);
    }
}

/// The subcommands a reader can use: not hidden and not the generated `help`.
fn visible_subcommands(command: &clap::Command) -> impl Iterator<Item = &clap::Command> {
    command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set() && subcommand.get_name() != "help")
}

/// The path followed by the command's positional arguments, required ones in angle brackets.
fn usage(command: &clap::Command, path: &str) -> String {
    let mut usage = path.to_string();
    for positional in command.get_positionals().filter(|arg| !arg.is_hide_set()) {
        let name = positional
            .get_value_names()
            .and_then(|names| names.first())
            .map_or_else(
                || positional.get_id().as_str().to_uppercase(),
                |n| n.to_string(),
            );
        let dots = if is_repeated(positional) { "..." } else { "" };
        if positional.is_required_set() {
            usage.push_str(&format!(" <{name}>{dots}"));
        } else {
            usage.push_str(&format!(" [{name}]{dots}"));
        }
    }
    usage
}

/// Reports whether the argument takes several values.
fn is_repeated(arg: &clap::Arg) -> bool {
    matches!(arg.get_action(), clap::ArgAction::Append)
        || arg.get_num_args().is_some_and(|n| n.max_values() > 1)
}

/// The first paragraph of the command's description collapsed onto one line.
fn about(command: &clap::Command) -> String {
    command
        .get_about()
        .map(|about| {
            let text = about.to_string();
            let paragraph = text.split("\n\n").next().unwrap_or_default();
            paragraph.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use clap::{Arg, Command};

    use super::render_commands;

    #[test]
    fn commands_are_grouped_padded_and_skip_hidden_and_help() {
        let mut root = Command::new("dotc")
            .subcommand(
                Command::new("model")
                    .about("Group about is not listed")
                    .subcommand(Command::new("list").about("List models"))
                    .subcommand(
                        Command::new("show")
                            .about("Show one model\n\nLong text")
                            .arg(Arg::new("model").required(true)),
                    )
                    .subcommand(Command::new("old").about("Hidden").hide(true)),
            )
            .subcommand(
                Command::new("dev").subcommand(
                    Command::new("workload").subcommand(
                        Command::new("launch")
                            .about("Launch a workload")
                            .arg(Arg::new("name")),
                    ),
                ),
            )
            .subcommand(Command::new("gc").about("Clean up"))
            .subcommand(Command::new("secret").about("Secret").hide(true));
        root.build();

        assert_eq!(
            render_commands(&root),
            "\
--- model ---
dotc model list                  # List models
dotc model show <MODEL>          # Show one model

--- dev ---
dotc dev workload launch [NAME]  # Launch a workload

--- gc ---
dotc gc                          # Clean up

Run `dotc <command> --help` for flags.
"
        );
    }
}
