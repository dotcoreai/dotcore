//! Starts a coding agent configured to use the site's inference API, with one inference route as
//! its active model.
//!
//! The command reads the routes from the inference API alone, chooses one, and then replaces
//! itself with the agent, which owns the terminal and whose exit code becomes the command's. It
//! works from any machine that can reach that API. Each agent receives the site's provider without
//! any change to the user's own agent configuration; files an agent needs live under the cache
//! folder.

mod address;
mod agent;
mod opencode;
mod pi;
mod routes;
mod selection;

use std::{ffi::OsString, io, os::unix::process::CommandExt, process::Command};

use anyhow::Context;
use clap::Args;
use dotcore_backend::composition::INFERENCE_ADDR_ENV;

use crate::{RootPathArgs, terminal::open_terminal};
use address::InferenceAddress;
pub(crate) use agent::CodingAgent;
use agent::LaunchSetup;
use routes::list_routes;
use selection::{find_route, pick_route};

/// Arguments for starting a coding agent.
#[derive(Debug, Args)]
pub struct CodeArgs {
    /// Coding agent to start; it must be installed on PATH.
    #[arg(value_name = "AGENT")]
    agent: CodingAgent,

    /// Inference route the agent starts with; without it, pick one on the terminal.
    #[arg(long, value_name = "ROUTE")]
    model: Option<String>,

    /// Inference API address, as host:port or http(s)://host[:port]; a route's Connect screen in
    /// the console shows it. Defaults to port 50052 on this machine.
    #[arg(long, env = INFERENCE_ADDR_ENV, value_name = "ADDRESS")]
    inference_addr: Option<InferenceAddress>,

    /// Arguments passed to the agent unchanged, after `--`.
    #[arg(last = true, value_name = "AGENT ARGS")]
    agent_args: Vec<OsString>,
}

impl CodeArgs {
    /// Starts the agent; returns only when it cannot be started.
    pub(crate) async fn dispatch(self, paths: &RootPathArgs) -> anyhow::Result<()> {
        let inference = self.inference_address();
        let routes = list_routes(&inference).await?;
        let route = self.choose_route(&routes)?;

        let launch = self.agent.launch(&LaunchSetup {
            base_url: inference.base_url(),
            routes: routes.iter().map(String::as_str).collect(),
            selected: route,
            passthrough: &self.agent_args,
            cache_root: &|| paths.cache_root(),
        })?;
        for file in &launch.files {
            file.ensure().with_context(|| {
                format!(
                    "cannot write {}, which {} needs",
                    file.path.display(),
                    self.agent.display_name()
                )
            })?;
        }

        tracing::debug!(
            agent = self.agent.name(),
            route,
            inference = %inference,
            "starting coding agent"
        );
        let error = Command::new(self.agent.name())
            .args(&launch.args)
            .envs(launch.env.iter().map(|(name, value)| (*name, value)))
            .exec();
        Err(start_failed(self.agent, error))
    }

    /// The given inference API address, else the default one on this machine.
    fn inference_address(&self) -> InferenceAddress {
        self.inference_addr
            .clone()
            .unwrap_or_else(InferenceAddress::local)
    }

    /// The named route, else the one picked on the terminal.
    fn choose_route<'a>(&self, routes: &'a [String]) -> anyhow::Result<&'a str> {
        match &self.model {
            Some(name) => Ok(find_route(self.agent, routes, name)?),
            // The picker's terminal is dropped at the end of this arm, restoring the terminal
            // mode before the agent takes it over.
            None => pick_route(self.agent, routes, open_terminal().as_mut()),
        }
    }
}

/// A failure the operator can act on when starting a coding agent.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CodeError {
    #[error(
        "cannot reach the inference API at {address}; pass its address with --inference-addr or \
         DOTCORE_INFERENCE_ADDR"
    )]
    InferenceUnreachable {
        address: InferenceAddress,
        #[source]
        source: reqwest::Error,
    },

    #[error("the inference API at {address} answered with status {status}")]
    InferenceRefused {
        address: InferenceAddress,
        status: u16,
    },

    #[error("the answer from {address} is not an inference route list")]
    InvalidRouteList {
        address: InferenceAddress,
        #[source]
        source: serde_json::Error,
    },

    #[error("the site has no ready inference route; create one, or wait for its model to be ready")]
    NoRoute,

    #[error("no ready inference route is named {name:?}; ready routes: {}", .ready.join(", "))]
    UnknownRoute {
        agent: CodingAgent,
        name: String,
        ready: Vec<String>,
    },

    #[error(
        "choosing an inference route needs a terminal; pass --model with one of: {}",
        .ready.join(", ")
    )]
    RouteRequired {
        agent: CodingAgent,
        ready: Vec<String>,
    },

    #[error(
        "{} is not installed: no `{}` executable on PATH; to install it: {}",
        .agent.display_name(),
        .agent.name(),
        .agent.install_hint()
    )]
    AgentNotInstalled { agent: CodingAgent },
}

/// Maps a failed start of the agent. Only a missing executable means the agent is not installed;
/// an executable that exists but cannot run, such as a script whose interpreter is missing, keeps
/// the system error.
fn start_failed(agent: CodingAgent, error: io::Error) -> anyhow::Error {
    if error.kind() == io::ErrorKind::NotFound && !on_path(agent.name()) {
        return CodeError::AgentNotInstalled { agent }.into();
    }
    anyhow::Error::new(error).context(format!("cannot start {}", agent.display_name()))
}

/// Reports whether a file named `executable` exists in a folder of the process search path.
fn on_path(executable: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|folder| folder.join(executable).is_file())
    })
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::*;
    use crate::{Cli, Command};

    #[test]
    fn the_command_takes_the_agent_its_flags_and_the_agent_arguments_after_a_separator() {
        // Flags given here override any address variables of the test environment.
        let cli = Cli::try_parse_from([
            "dotc",
            "code",
            "opencode",
            "--model",
            "r",
            "--inference-addr",
            "0.0.0.0:50052",
            "--",
            "--foo",
            "bar",
        ])
        .expect("code command");

        let Command::Code(code) = cli.command else {
            panic!("code command");
        };
        assert_eq!(code.agent, CodingAgent::Opencode);
        assert_eq!(code.model.as_deref(), Some("r"));
        assert_eq!(
            code.inference_addr.map(|address| address.to_string()),
            Some("http://127.0.0.1:50052".to_owned())
        );
        assert_eq!(code.agent_args, ["--foo", "bar"].map(OsString::from));
    }

    #[test]
    fn the_command_rejects_an_unknown_agent_and_arguments_without_a_separator() {
        let error = Cli::try_parse_from(["dotc", "code", "claude"]).expect_err("unknown agent");
        assert_eq!(error.kind(), ErrorKind::InvalidValue);
        assert_eq!(error.exit_code(), 2);

        let error = Cli::try_parse_from(["dotc", "code", "opencode", "run"])
            .expect_err("agent arguments need a separator");
        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }

    #[test]
    fn the_command_takes_no_manager_address() {
        let error =
            Cli::try_parse_from(["dotc", "code", "opencode", "--manager", "10.0.0.5:50051"])
                .expect_err("no manager option");

        assert_eq!(error.kind(), ErrorKind::UnknownArgument);
    }
}
