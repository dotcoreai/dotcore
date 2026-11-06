//! `dotc hardware`: describes this machine's hardware and recommends models for it.
//!
//! Both commands work without a site or a running server. `show` prints the machine as a
//! description other tools can load, and `recommend` ranks the master catalog's models for this
//! machine or for a description file. Results go to stdout in a stable plain form.

mod recommend;
mod show;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use dotcore_catalog::{Catalog, OciArtifactPolicy};
use std::path::Path;

use crate::RootPathArgs;
use recommend::{RecommendArgs, recommend};
use show::show;

/// Commands that describe the hardware of a machine, with no site involved.
#[derive(Debug, Args)]
#[command(arg_required_else_help = true)]
pub struct HardwareCommand {
    #[command(subcommand)]
    command: Option<HardwareSubcommand>,
}

/// Actions supported by the hardware command group.
#[derive(Debug, Subcommand)]
enum HardwareSubcommand {
    /// Print this machine's hardware as a machine description, with the hardware class of each
    /// model target. The output has no host name, user name, path or address, so it is safe to
    /// paste into a bug report.
    Show,

    /// Rank the master catalog's models for each use case on this machine, or on the machine
    /// description in a file.
    Recommend(RecommendArgs),
}

impl HardwareCommand {
    pub(crate) fn dispatch(self, paths: &RootPathArgs) -> Result<()> {
        match self.command {
            Some(HardwareSubcommand::Show) => show(paths),
            Some(HardwareSubcommand::Recommend(arguments)) => recommend(paths, arguments),
            None => anyhow::bail!("hardware command requires an action"),
        }
    }
}

/// Loads the master catalog folder the way the server does.
fn load_catalog(catalog_dir: &Path) -> Result<Catalog> {
    Catalog::load_with_policy(
        catalog_dir,
        OciArtifactPolicy::Development,
        dotcore::model::engine::declaration_validator(),
    )
    .with_context(|| format!("cannot load the master catalog {}", catalog_dir.display()))
}
