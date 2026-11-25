//! Read-only inspection of the effective process paths and node host, and the shared reader of
//! process variables.
//!
//! The configuration command reports path policy and the node host a manager site records,
//! without mutating the site or creating directories.

use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Subcommand};
use dotcore::composition::{NodeRole, node_role};
use dotcore_domain::ResolvedSitePaths;
use serde::Serialize;

use crate::{
    RootPathArgs,
    output::{OutputFormat, write_plain, write_yaml},
    site::recorded_host,
    terminal::Message,
};

/// Commands that inspect effective Dotcore process configuration.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct ConfigCommand {
    #[command(subcommand)]
    command: Option<ConfigSubcommand>,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    /// Show the effective process paths and this site folder's role
    ///
    /// Also shows the node host a manager site records. Nothing is changed.
    Show,
}

impl ConfigCommand {
    pub(crate) fn dispatch(self, paths: &RootPathArgs, format: OutputFormat) -> Result<()> {
        let Some(ConfigSubcommand::Show) = self.command else {
            anyhow::bail!("config command requires an action");
        };
        let document = ConfigDocument::of(paths.process_paths()?);
        if format == OutputFormat::Yaml {
            return write_yaml(&document);
        }
        write_plain(&[Message::KeyValues(document.pairs())])
    }
}

/// The effective process configuration: where the process keeps its files and what role the
/// folder gives it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigDocument {
    role: &'static str,
    site_dir: PathBuf,

    /// The master catalog folder, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    master_catalog_dir: Option<PathBuf>,

    cache_dir: PathBuf,
    model_artifact_cache_dir: PathBuf,
    workload_cache_dir: PathBuf,

    /// The node host a manager site records; absent for a joined node or an uninitialized site.
    #[serde(skip_serializing_if = "Option::is_none")]
    node_host: Option<String>,
}

impl ConfigDocument {
    fn of(paths: ResolvedSitePaths) -> Self {
        let (role, node_host) = match node_role(&paths.root) {
            NodeRole::Manager => ("manager", recorded_host(&paths.root)),
            // A joined node folder holds no site inputs, so it records no node host here.
            NodeRole::JoinedNode => ("joined-node", None),
        };
        Self {
            role,
            model_artifact_cache_dir: paths.model_artifact_cache_root(),
            workload_cache_dir: paths.workload_cache_root(),
            site_dir: paths.root,
            master_catalog_dir: paths.master_catalog_root,
            cache_dir: paths.cache_root,
            node_host: node_host.map(|host| host.to_string()),
        }
    }

    /// The facts as labeled values, without the node host when none is recorded.
    fn pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("role".to_owned(), self.role.to_owned()),
            (
                "site folder".to_owned(),
                self.site_dir.display().to_string(),
            ),
            (
                "cache folder".to_owned(),
                self.cache_dir.display().to_string(),
            ),
            (
                "model artifact cache folder".to_owned(),
                self.model_artifact_cache_dir.display().to_string(),
            ),
            (
                "workload cache folder".to_owned(),
                self.workload_cache_dir.display().to_string(),
            ),
        ];
        if let Some(catalog) = &self.master_catalog_dir {
            pairs.insert(
                2,
                (
                    "master catalog folder".to_owned(),
                    catalog.display().to_string(),
                ),
            );
        }
        if let Some(host) = &self.node_host {
            pairs.push(("node host".to_owned(), host.clone()));
        }
        pairs
    }
}

/// Reads one process variable: `None` when it is unset, an error when it is not valid UTF-8.
pub(crate) fn environment_value(variable: &'static str) -> Result<Option<String>> {
    match std::env::var(variable) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!("could not read {variable}"))),
    }
}
