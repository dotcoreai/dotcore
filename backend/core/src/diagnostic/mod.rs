//! Reads the retained backend logs of every SiteNode, on demand, for the site manager's
//! management diagnostics.
//!
//! Each node reads only its own retained files, removes filesystem paths, and bounds every
//! response; a node in another process serves the manager on its private listener, fenced by
//! manager session and node incarnation. The manager merges machines into one timeline, reports
//! an unreachable machine as unavailable instead of failing, and retains nothing. Log data passes
//! through neither node agent nor observability.

mod node;
mod retained;
mod site;

use std::time::Duration;

pub(crate) use node::{NodeDiagnosticReader, NodeDiagnosticService};
pub(crate) use retained::redact_filesystem_paths;
pub(crate) use site::{NodeDiagnostics, SiteDiagnosticService};

/// Largest encoded history page a node or the manager returns.
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Largest encoded private diagnostic message in either direction: a full history page plus room
/// for its framing.
const MESSAGE_LIMIT_BYTES: usize = MAX_RESPONSE_BYTES + 64 * 1024;

/// Largest number of events one history page returns.
const MAX_PAGE_SIZE: u32 = 200;

/// Longest a manager waits for a node's history, or for the first frame of a follow, so a hung
/// node cannot hold a management request.
/// Not a behavior duration: bound on a real call to a node's diagnostics service.
const NODE_DIAGNOSTIC_DEADLINE: Duration = Duration::from_secs(10);

/// Resolves a requested page size; 0 selects the default and sizes above 200 are invalid.
fn resolve_page_size(page_size: u32) -> Option<usize> {
    crate::management_logs::resolve_page_size(
        page_size,
        crate::management_logs::DEFAULT_LOG_PAGE_SIZE,
        MAX_PAGE_SIZE,
    )
    .map(|page_size| page_size as usize)
}
