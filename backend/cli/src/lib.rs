//! Command parsing and dispatch for the product CLI.
//!
//! It defines the command tree, parses the process arguments into it, and routes each command to
//! its owning capability. Failures are rendered as a verdict with next steps on a terminal and as a
//! plain-text cause chain otherwise, and an interrupted guided prompt exits distinctly from an
//! ordinary error.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("Dotcore supports only Linux and macOS targets");

use std::{
    io::{self, Write},
    process::ExitCode,
};

use anyhow::bail;
use anyhow::{Context, Result};
use clap::{Args, CommandFactory, Parser, Subcommand};
use tokio::runtime::{Builder, Runtime};

mod application;
mod code;
mod config;
mod failure;
mod gc;
pub mod guided;
mod hardware;
mod hint;
mod hosts;
mod management;
mod model;
mod node;
mod parameters;
mod paths;
mod runtime_confirmation;
mod server;
mod site;
mod telemetry;
pub mod terminal;
#[cfg(test)]
mod test_support;
mod workload;

pub use paths::RootPathArgs;

/// The top-level command tree for `dotc`.
#[derive(Debug, Parser)]
#[command(
    name = "dotc",
    version,
    arg_required_else_help = true,
    subcommand_required = true
)]
pub struct Cli {
    /// Show the complete cause chain of a failure on a terminal.
    #[arg(long, global = true)]
    pub verbose: bool,

    #[command(flatten)]
    pub paths: RootPathArgs,

    #[command(subcommand)]
    pub command: Command,
}

/// Product CLI command groups.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect effective Dotcore process configuration.
    Config(config::ConfigCommand),

    /// Manage the bare local server process that `dotcore` supervises.
    Server(ServerCommand),

    /// Create, reset, and validate offline site inputs, and add nodes to a stopped site.
    Site(site::SiteCommand),

    /// Manage site nodes: `add`, `remove`, and `list` on the manager, `join` and `leave` on the
    /// node's machine.
    Node(node::NodeCommand),

    /// Manage the model deployments of the running site: `list`, `show`, `pause`, `resume`,
    /// `teardown` and `logs`.
    #[command(alias = "models")]
    Model(model::ModelCommand),

    /// Manage the application deployments of the running site: `list`, `show`, `pause`, `resume`,
    /// `teardown` and `logs`.
    #[command(visible_alias = "app", alias = "applications")]
    Application(application::ApplicationCommand),

    /// Start a coding agent on a site's inference API, with an inference route as its model.
    Code(code::CodeArgs),

    /// Run one transient development workload.
    Dev(workload::DevCommand),

    /// Remove Docker image names on this machine that no Docker-delivered Nix artifact uses,
    /// after listing them and asking for confirmation.
    Gc(gc::GcCommand),

    /// Describe this machine's hardware and recommend models for it, without a site: `show` and
    /// `recommend`.
    Hardware(hardware::HardwareCommand),
}

/// Commands for the local server process.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ServerCommand {
    #[command(subcommand)]
    command: Option<ServerSubcommand>,
}

/// Actions supported by the local server command group.
#[derive(Debug, Subcommand)]
enum ServerSubcommand {
    /// Run the bare backend server process for a site folder; settings come from the environment.
    /// `dotcore` supervises this process and also serves the console and public listeners.
    Start,
}

/// Initializes the process-wide tracing subscriber. The interactive node membership, model and application commands
/// default to warnings so their prompts and result records are not interleaved with routine
/// events; `RUST_LOG` overrides this.
pub fn initialize_tracing() {
    let words = command_words(std::env::args_os());
    dotcore::logging::initialize_with_default_filter(if quiet_by_default(&words) {
        "warn"
    } else {
        "info"
    });
}

/// Builds the one Tokio runtime used by the CLI process.
pub fn build_runtime() -> Result<Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build the CLI Tokio runtime")
}

/// Dispatches a parsed command to its owning capability.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let paths = &cli.paths;
    match cli.command {
        Command::Config(command) => command.dispatch(paths),
        Command::Server(command) => command.dispatch(paths).await,
        Command::Site(command) => command.dispatch(paths).await,
        Command::Node(command) => command.dispatch(paths).await,
        Command::Model(command) => command.dispatch().await,
        Command::Application(command) => command.dispatch().await,
        Command::Code(command) => command.dispatch(paths).await,
        Command::Dev(command) => command.dispatch(paths).await,
        Command::Gc(command) => command.dispatch(paths).await,
        Command::Hardware(command) => command.dispatch(paths),
    }
}

/// Reports a command failure: laid out as a verdict, facts and next steps when stderr is a
/// terminal, else as the plain single-line cause chain. `verbose` adds the cause chain to the
/// terminal layout.
pub fn report_failure(error: anyhow::Error, verbose: bool) -> ExitCode {
    if let Some(error) = error.downcast_ref::<workload::WorkloadCommandError>() {
        return error.exit_code();
    }
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<terminal::PromptError>()
            .is_some_and(|error| matches!(error, terminal::PromptError::Cancelled))
    }) {
        return ExitCode::from(130);
    }
    let messages = failure::Failure::from_error(&error).messages(verbose);
    // A closed stderr loses the report; the exit code still tells the failure.
    let mut stderr = io::stderr().lock();
    let _ = match terminal::render_for_stderr(&messages) {
        Some(text) => write!(stderr, "{text}"),
        None => writeln!(stderr, "{}", format_failure(&error)),
    };
    ExitCode::from(1)
}

/// Formats a command failure as one line: every distinct source in its cause chain, each
/// followed by the command hint that completes its remedy.
pub fn format_failure(error: &anyhow::Error) -> String {
    failure::chain_line(error)
}

/// Returns the first three arguments after the program name that are not root options, which
/// name the command group, the action and the first argument of the action. Root options may
/// precede or interleave them.
fn command_words(arguments: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<String> {
    let mut words = Vec::new();
    let mut arguments = arguments.into_iter().skip(1);
    while let Some(argument) = arguments.next() {
        let argument = argument.to_string_lossy();
        if paths::VALUE_OPTIONS.contains(&argument.as_ref()) {
            arguments.next();
        } else if argument == "--verbose"
            || paths::VALUE_OPTIONS.iter().any(|option| {
                argument
                    .strip_prefix(option)
                    .is_some_and(|rest| rest.starts_with('='))
            })
        {
            continue;
        } else {
            words.push(argument.into_owned());
            if words.len() == 3 {
                break;
            }
        }
    }
    words
}

/// Returns the arguments without the root path options and their values.
fn without_root_paths(arguments: &[std::ffi::OsString]) -> Vec<std::ffi::OsString> {
    let mut kept = Vec::new();
    let mut arguments = arguments.iter();
    while let Some(argument) = arguments.next() {
        let text = argument.to_string_lossy();
        if paths::VALUE_OPTIONS.contains(&text.as_ref()) {
            arguments.next();
        } else if !paths::VALUE_OPTIONS.iter().any(|option| {
            text.strip_prefix(option)
                .is_some_and(|rest| rest.starts_with('='))
        }) {
            kept.push(argument.clone());
        }
    }
    kept
}

/// Reports whether the command prints prompts and result records that routine info events would
/// interleave with, so it stays at warnings.
fn quiet_by_default(words: &[String]) -> bool {
    matches!(
        (
            words.first().map(String::as_str),
            words.get(1).map(String::as_str)
        ),
        (
            Some("node"),
            Some("add" | "join" | "remove" | "leave" | "list")
        ) | (
            Some("model" | "application" | "app" | "code" | "gc" | "hardware"),
            _
        )
    )
}

/// Reports whether the command is a development workload action, which reports failures as
/// JSON.
fn workload_arguments(words: &[String]) -> bool {
    words.first().is_some_and(|word| word == "dev")
        && words.get(1).is_some_and(|word| word == "workload")
}

impl ServerCommand {
    async fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        let Some(ServerSubcommand::Start) = self.command else {
            bail!("server command requires an action")
        };

        server::start(&paths.server_overrides()?)
            .await
            .context("server start failed")
    }
}

impl Cli {
    /// Parses the CLI and applies argument relationships that depend on values.
    pub fn try_parse_from<I, T>(arguments: I) -> Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let arguments = arguments.into_iter().map(Into::into).collect::<Vec<_>>();
        Self::reject_missing_action(&arguments)?;
        let matches = Self::command().try_get_matches_from(arguments)?;
        let mut cli = <Self as clap::FromArgMatches>::from_arg_matches(&matches)?;
        cli.paths.site_dir_on_command_line =
            matches.value_source("site_dir") == Some(clap::parser::ValueSource::CommandLine);
        if let Some(message) = cli.argument_contract_error() {
            return Err(Self::command().error(clap::error::ErrorKind::ArgumentConflict, message));
        }
        Ok(cli)
    }

    /// Parses process arguments and exits using clap's standard error contract.
    pub fn parse() -> Self {
        match Self::try_parse_from(std::env::args_os()) {
            Ok(cli) => cli,
            Err(error) => error.exit(),
        }
    }

    /// Parses process arguments and gives workload parse failures the workload JSON protocol.
    pub fn parse_process() -> Result<Self, ExitCode> {
        let arguments = std::env::args_os().collect::<Vec<_>>();
        let words = command_words(arguments.clone());
        match Self::try_parse_from(arguments) {
            Ok(cli) => Ok(cli),
            Err(error) if workload_arguments(&words) && error.exit_code() != 0 => {
                eprint!("{error}");
                let operation = words
                    .get(2)
                    .map(String::as_str)
                    .filter(|operation| {
                        matches!(
                            *operation,
                            "launch" | "inspect" | "logs" | "vram" | "teardown"
                        )
                    })
                    .unwrap_or("workload");
                println!(
                    "{}",
                    serde_json::json!({"format_version": 1, "operation": operation, "result": "error", "error": {"code": "invalid_arguments", "message": error.to_string()}})
                );
                Err(ExitCode::from(error.exit_code() as u8))
            }
            Err(error) => error.exit(),
        }
    }

    /// Shows help for a command group named without an action, as when no environment variable
    /// is set and no root path option is given. Clap counts both as given arguments, which would
    /// otherwise run the bare group instead.
    fn reject_missing_action(arguments: &[std::ffi::OsString]) -> Result<(), clap::Error> {
        // The environment is replaced by its help annotation, so the help reads the same.
        let without_environment = Self::command().mut_args(|argument| {
            let Some(variable) = argument
                .get_env()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                return argument;
            };
            let help = argument
                .get_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            argument.env(None).help(format!("{help} [env: {variable}]"))
        });
        match without_environment.try_get_matches_from(without_root_paths(arguments)) {
            Err(error)
                if error.kind()
                    == clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand =>
            {
                Err(error)
            }
            _ => Ok(()),
        }
    }

    fn argument_contract_error(&self) -> Option<&'static str> {
        match &self.command {
            Command::Site(command) => command.argument_contract_error(),
            Command::Node(command) => command.argument_contract_error(),
            Command::Model(command) => command.argument_contract_error(),
            Command::Application(command) => command.argument_contract_error(),
            Command::Config(_)
            | Command::Server(_)
            | Command::Code(_)
            | Command::Dev(_)
            | Command::Gc(_)
            | Command::Hardware(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::Cli;

    #[test]
    fn server_start_is_the_canonical_server_command() {
        let command = Cli::try_parse_from(["dotc", "server", "start"]).expect("server start");
        assert!(matches!(command.command, super::Command::Server(_)));
    }

    #[test]
    fn server_start_takes_the_site_dir_before_or_after_the_command() {
        for arguments in [
            ["dotc", "server", "start", "--site-dir", "/srv/node"],
            ["dotc", "--site-dir", "/srv/node", "server", "start"],
        ] {
            let command = Cli::try_parse_from(arguments).expect("server start with a folder");
            assert_eq!(
                command.paths.site_dir,
                Some(std::path::PathBuf::from("/srv/node")),
                "{arguments:?}"
            );
            let super::Command::Server(server) = command.command else {
                panic!("server command");
            };
            assert!(matches!(
                server.command,
                Some(super::ServerSubcommand::Start)
            ));
        }
    }

    #[test]
    fn command_words_skip_root_options_and_their_values() {
        let words = |arguments: &[&str]| {
            super::command_words(arguments.iter().map(std::ffi::OsString::from))
        };

        assert_eq!(
            words(&["dotc", "node", "add", "--site-dir", "/a"]),
            ["node", "add"]
        );
        assert_eq!(
            words(&["dotc", "--site-dir", "/a", "node", "add"]),
            ["node", "add"]
        );
        assert_eq!(
            words(&["dotc", "--site-dir=/a", "node", "add"]),
            ["node", "add"]
        );
        assert_eq!(
            words(&[
                "dotc",
                "--verbose",
                "--catalog-dir",
                "/c",
                "--cache-dir=/k",
                "dev",
                "workload",
                "launch"
            ]),
            ["dev", "workload", "launch"]
        );
        assert_eq!(
            words(&["dotc", "node", "--site-dir", "/a", "list"]),
            ["node", "list"]
        );
        assert_eq!(words(&["dotc"]), Vec::<String>::new());
    }

    #[test]
    fn root_options_do_not_change_the_quiet_default_of_a_command() {
        let quiet = |arguments: &[&str]| {
            super::quiet_by_default(&super::command_words(
                arguments.iter().map(std::ffi::OsString::from),
            ))
        };

        assert!(quiet(&["dotc", "node", "add", "--site-dir", "/a"]));
        assert!(quiet(&["dotc", "--site-dir", "/a", "node", "add"]));
        assert!(quiet(&["dotc", "--site-dir=/a", "model", "list"]));
        assert!(quiet(&["dotc", "--cache-dir", "/k", "app", "ls"]));
        assert!(quiet(&["dotc", "code", "opencode"]));
        assert!(!quiet(&["dotc", "--site-dir", "/a", "site", "init"]));
        assert!(!quiet(&["dotc", "--site-dir", "/a", "node", "join-ish"]));
        assert!(!quiet(&["dotc", "server", "start"]));
    }

    #[test]
    fn root_options_do_not_hide_a_workload_command() {
        let workload = |arguments: &[&str]| {
            super::workload_arguments(&super::command_words(
                arguments.iter().map(std::ffi::OsString::from),
            ))
        };

        assert!(workload(&["dotc", "dev", "workload", "launch"]));
        assert!(workload(&[
            "dotc",
            "--catalog-dir",
            "/c",
            "dev",
            "workload",
            "launch"
        ]));
        assert!(!workload(&[
            "dotc",
            "--catalog-dir",
            "/c",
            "dev",
            "telemetry",
            "fit"
        ]));
    }

    #[test]
    fn site_init_is_a_nested_command() {
        let command = Cli::try_parse_from([
            "dotc",
            "site",
            "init",
            "--site-dir",
            "/tmp/site",
            "--inference",
            "simulated",
            "--applications",
            "simulated",
        ])
        .expect("site init");
        assert!(matches!(command.command, super::Command::Site(_)));
    }

    #[test]
    fn model_and_application_groups_accept_their_synonyms() {
        for (arguments, is_model) in [
            (["dotc", "model", "ls"], true),
            (["dotc", "models", "list"], true),
            (["dotc", "app", "ls"], false),
            (["dotc", "applications", "list"], false),
        ] {
            let command = Cli::try_parse_from(arguments).expect("synonym parses");
            match (command.command, is_model) {
                (super::Command::Model(_), true) | (super::Command::Application(_), false) => {}
                (other, _) => panic!("{arguments:?} parsed as {other:?}"),
            }
        }
    }

    #[test]
    fn plain_cli_shows_help_as_invalid_usage() {
        assert_help_error(["dotc"]);
    }

    #[test]
    fn incomplete_server_shows_help_as_invalid_usage() {
        assert_help_error(["dotc", "server"]);
    }

    #[test]
    fn unsupported_server_aliases_fail() {
        for arguments in [["dotc", "start"], ["dotc", "serve"]] {
            let error = Cli::try_parse_from(arguments).expect_err("unsupported alias");
            assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
            assert_eq!(error.exit_code(), 2);
        }
        let error = Cli::try_parse_from(["dotc", "server", "run"]).expect_err("unsupported alias");
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn help_and_version_succeed() {
        for arguments in [["dotc", "--help"], ["dotc", "--version"]] {
            let error = Cli::try_parse_from(arguments).expect_err("clap display command");
            assert!(matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ));
            assert_eq!(error.exit_code(), 0);
        }

        let error = Cli::try_parse_from(["dotc", "--version"]).expect_err("version");
        assert!(error.to_string().contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn help_output_names_the_nested_server_start_command() {
        let error = Cli::try_parse_from(["dotc", "--help"]).expect_err("help");
        let output = error.to_string();
        assert!(output.contains("server"));
        assert!(output.contains("Usage: dotc"));
    }

    #[test]
    fn failures_use_exit_one_and_include_the_full_cause_chain() {
        let error = anyhow::anyhow!("leaf failure").context("server start failed");
        assert_eq!(
            super::report_failure(error, false),
            std::process::ExitCode::from(1)
        );
        let error = anyhow::anyhow!("leaf failure").context("server start failed");
        assert_eq!(
            super::format_failure(&error),
            "server start failed: leaf failure"
        );
    }

    #[test]
    fn local_administrator_failures_include_complete_configuration_diagnostics() {
        let error = anyhow::anyhow!(
            "upstream request to https://private.example/v1 with api_key=test-upstream-key failed"
        )
        .context("server start failed");

        assert_eq!(
            super::format_failure(&error),
            "server start failed: upstream request to https://private.example/v1 with api_key=test-upstream-key failed"
        );
    }

    #[test]
    fn guided_interrupt_uses_exit_130_without_reporting_a_failure() {
        let error = anyhow::Error::new(crate::terminal::PromptError::Cancelled);
        assert_eq!(
            super::report_failure(error, false),
            std::process::ExitCode::from(130)
        );
    }

    #[test]
    fn guided_end_of_input_uses_exit_one_with_the_cause_chain() {
        let error = anyhow::Error::new(crate::terminal::PromptError::EndOfInput)
            .context("guided site initialization");
        assert_eq!(
            super::report_failure(error, false),
            std::process::ExitCode::from(1)
        );
        assert_eq!(
            super::format_failure(
                &anyhow::Error::new(crate::terminal::PromptError::EndOfInput)
                    .context("guided site initialization")
            ),
            "guided site initialization: guided interaction reached end of input"
        );
    }

    fn assert_help_error<const N: usize>(arguments: [&str; N]) {
        let error = Cli::try_parse_from(arguments).expect_err("help error");
        assert_eq!(
            error.kind(),
            ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
        );
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("Usage:"));
    }
}
