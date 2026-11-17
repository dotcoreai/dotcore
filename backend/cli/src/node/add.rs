//! Adding a node to a site.
//!
//! The site manager hosts each join while it runs; this command moves onto its management API.
//! Until then it refuses to run, so no offline path records a node next to a live manager.

use std::{path::PathBuf, time::Duration};

use clap::Args;
use dotcore_domain::BindAddress;

use super::parse_timeout;
use crate::{RootPathArgs, site::parse_absolute_path};

/// Arguments of `dotc node add`.
#[derive(Debug, Args)]
pub struct NodeAddArgs {
    /// IPv4 address or host name and port of the one-time join listener; defaults to the
    /// manager's private address. A host name is resolved by this machine.
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<BindAddress>,

    /// Port of the manager's private listener, which joined nodes dial on the manager address
    /// they reach it at; defaults to the DOTCORE_PRIVATE_ADDR port, else 50053. The manager
    /// listens on 50053 by default once nodes have joined, so another port needs
    /// DOTCORE_PRIVATE_ADDR (or `dotcore --private-addr`) when the manager starts.
    #[arg(long, value_name = "PORT", value_parser = clap::value_parser!(u16).range(1..))]
    pub manager_private_port: Option<u16>,

    /// How long the join token stays valid, for example 90s, 15m, or 2h.
    #[arg(long, value_name = "DURATION", default_value = "15m", value_parser = parse_timeout)]
    pub timeout: Duration,

    /// Write the token to this new file (mode 0600, removed when the command ends) instead of
    /// printing it.
    #[arg(long, value_name = "PATH", value_parser = parse_absolute_path)]
    pub token_file: Option<PathBuf>,

    /// Allow the node that presents the token without asking.
    #[arg(long)]
    pub yes: bool,
}

/// Refuses to add a node until the command runs against the running manager.
pub(crate) async fn node_add(_paths: &RootPathArgs, _arguments: NodeAddArgs) -> anyhow::Result<()> {
    anyhow::bail!("adding a node through the running manager is not available in this build yet")
}
