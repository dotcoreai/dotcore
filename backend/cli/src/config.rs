//! Read-only inspection of the effective process paths and node host, and the shared reader of
//! process variables.
//!
//! The configuration command reports path policy and the node host a manager site records,
//! without mutating the site or creating directories.

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use dotcore::composition::{NodeRole, node_role};
use dotcore_domain::ResolvedSitePaths;

use crate::{RootPathArgs, site::recorded_host};

/// Commands that inspect effective Dotcore process configuration.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ConfigCommand {
    #[command(subcommand)]
    command: Option<ConfigSubcommand>,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    /// Show effective process paths, the node role (`manager` or `joined-node`), and the node
    /// host a manager site records, without mutating the site.
    Show(ConfigShowArgs),
}

#[derive(Debug, Args)]
struct ConfigShowArgs {
    /// Select the machine-readable output representation.
    #[arg(long, value_enum)]
    format: ConfigFormat,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ConfigFormat {
    Json,
}

impl ConfigCommand {
    pub(crate) fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        let Some(ConfigSubcommand::Show(arguments)) = self.command else {
            anyhow::bail!("config command requires an action");
        };
        match arguments.format {
            ConfigFormat::Json => show_json(paths.process_paths()?),
        }
    }
}

fn show_json(paths: ResolvedSitePaths) -> Result<()> {
    let (role, node_host) = match node_role(&paths.root) {
        NodeRole::Manager => ("manager", recorded_host(&paths.root)),
        // A joined node folder holds no site inputs, so it records no node host here.
        NodeRole::JoinedNode => ("joined-node", None),
    };
    let output = serde_json::json!({
        "role": role,
        "siteDir": paths.root,
        "masterCatalogDir": paths.master_catalog_root,
        "cacheDir": paths.cache_root,
        "modelArtifactCacheDir": paths.model_artifact_cache_root(),
        "workloadCacheDir": paths.workload_cache_root(),
        "nodeHost": node_host.map(|host| host.to_string()),
    });
    println!("{output}");
    Ok(())
}

/// Reads one process variable: `None` when it is unset, an error when it is not valid UTF-8.
pub(crate) fn environment_value(variable: &'static str) -> Result<Option<String>> {
    match std::env::var(variable) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!("could not read {variable}"))),
    }
}
