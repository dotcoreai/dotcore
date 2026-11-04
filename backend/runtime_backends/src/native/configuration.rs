//! Settings of one native execution target: where its workload state lives, which store and
//! artifact roots its workloads may use, the site identities its ownership evidence records, and
//! whether its workloads outlive the server process.

use std::path::PathBuf;

use dotcore_domain::SiteNodeId;

use super::NativeStateRoot;

/// Store root that native executables must come from when composition sets no other.
pub const DEFAULT_STORE_ROOT: &str = "/nix/store";

/// Site-local native runtime settings that do not enter workload owner contracts.
#[derive(Clone, Debug)]
pub struct NativeRuntimeConfig {
    /// Native workload state of the site, `<site dir>/runtime/native`.
    pub state_root: NativeStateRoot,
    /// Directory whose top-level entries are the only executable artifacts, normally `/nix/store`.
    pub store_root: PathBuf,
    /// Directory every artifact binding source must be inside.
    pub approved_artifact_root: PathBuf,
    pub site_id: String,
    pub site_node_id: SiteNodeId,
    pub execution_target_id: String,
    pub lifetime: NativeWorkloadLifetime,
}

/// How long native workloads may outlive the server process that created them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NativeWorkloadLifetime {
    /// Workloads keep running across server restarts and are adopted again.
    #[default]
    Persistent,

    /// Workloads stop gracefully once the server process that created them exits.
    Ephemeral,
}
