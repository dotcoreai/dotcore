//! Stops and removes the native workloads of one site directory or joined node folder before that
//! directory is removed, by a site reset, a site deletion, or a node leaving its site, so no
//! launcher-held process outlives its state.
//!
//! Cleanup is scoped by location, not by site configuration: it covers every execution target
//! under `<dir>/runtime/native`, even when the site identity is unknown. Workloads recorded under
//! another state root, such as those of a copied directory, are left alone. Contradictory or
//! unreadable evidence under the directory's own root refuses the removal before anything is
//! stopped.

use std::{
    fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
};

use dotcore_domain::{SiteNodeId, valid_id};
use dotcore_launcher::supervisor::{
    INTERRUPTED_CREATE_AGE, Inconsistency, LaunchPayload, ScannedWorkload, Supervisor,
    SupervisorError, WorkloadState,
};
use dotcore_runtime::{LocatedWorkloadCleanup, RuntimeBackendError};
use tracing::{info, warn};

use super::{
    NativeStateRoot,
    adapter::{stop_and_remove, supervisor_error},
    ownership::{ExpectedOwner, NativeOwnershipError, NativeWorkloadClass, classify_for_removal},
};

/// Runtime name shown beside each native workload a removal lists.
pub const NATIVE_RUNTIME_NAME: &str = "Native runtime";

/// Native workload cleanup of one site directory or node folder, held exclusively while its server
/// is stopped.
#[derive(Debug)]
pub struct NativeSiteCleanup {
    site_root: PathBuf,
    state_root: NativeStateRoot,
    site_id: Option<String>,
    site_node_id: Option<SiteNodeId>,
}

impl NativeSiteCleanup {
    /// Selects the native state root of the directory `site_root`. A known site or SiteNode
    /// identity must match every owned workload's record; unknown identities must at least agree
    /// across the root.
    pub fn new(
        site_root: &Path,
        site_id: Option<String>,
        site_node_id: Option<SiteNodeId>,
    ) -> Self {
        Self {
            site_root: site_root.to_owned(),
            state_root: NativeStateRoot::for_site(site_root),
            site_id,
            site_node_id,
        }
    }

    /// Describes each owned workload whose launcher may still hold a process, without changing
    /// anything. Fails when any such workload under the root cannot be conclusively classified.
    pub async fn inventory(&self) -> Result<Vec<String>, NativeSiteCleanupError> {
        Ok(self
            .owned_workloads()
            .await?
            .iter()
            .map(OwnedWorkload::description)
            .collect())
    }

    /// Stops each owned workload, waiting for launchers that are already draining, and removes
    /// its directory. Every workload is classified before the first one is stopped.
    pub async fn cleanup(&self) -> Result<(), NativeSiteCleanupError> {
        for workload in self.owned_workloads().await? {
            stop_and_remove(
                &workload.supervisor,
                &workload.target_directory,
                &workload.id,
            )
            .await?;
            info!(event_kind = "native_site_workload_removed", workload_id = %workload.id, execution_target_id = %workload.execution_target_id, "stopped and removed a native workload of the removed directory");
        }
        Ok(())
    }

    /// Classifies every workload that may hold a process. Finished workloads and interrupted
    /// creates hold none and go with the removed directory, so their evidence is not consulted.
    async fn owned_workloads(&self) -> Result<Vec<OwnedWorkload>, NativeSiteCleanupError> {
        let Some(canonical_root) = self.canonical_root()? else {
            return Ok(Vec::new());
        };
        let mut owned = Vec::new();
        // Site and SiteNode of the first owned workload, which every other one must repeat
        // when the caller does not know them.
        let mut first_owner: Option<(String, SiteNodeId)> = None;
        for (execution_target_id, target_directory) in self.target_directories(&canonical_root)? {
            // Cleanup never starts a workload, so it needs no launcher binary.
            let supervisor = Supervisor::new(&target_directory, PathBuf::new())
                .map_err(|error| supervisor_error("open native workload state", error))?;
            let scanned = supervisor
                .scan()
                .await
                .map_err(|error| supervisor_error("scan native workloads", error))?;
            for ScannedWorkload { id, state } in scanned {
                let directory = target_directory.join(&id);
                let (payload, state) = match state {
                    WorkloadState::Exited { .. }
                    | WorkloadState::Absent { .. }
                    | WorkloadState::Interrupted => continue,
                    WorkloadState::Creating => {
                        return Err(NativeSiteCleanupError::CreateInProgress { directory });
                    }
                    WorkloadState::Inconsistent { problem } => (Err(problem), "inconsistent"),
                    WorkloadState::Running { payload } => (Ok(payload), "running"),
                    WorkloadState::Terminating => {
                        (read_payload(&supervisor, &id).await?, "stopping")
                    }
                    WorkloadState::Lost => (read_payload(&supervisor, &id).await?, "launcher lost"),
                };
                let expected = ExpectedOwner {
                    site_id: self
                        .site_id
                        .as_deref()
                        .or(first_owner.as_ref().map(|(site_id, _)| site_id.as_str())),
                    site_node_id: self
                        .site_node_id
                        .as_ref()
                        .or(first_owner.as_ref().map(|(_, site_node_id)| site_node_id)),
                    execution_target_id: &execution_target_id,
                };
                let class = match payload {
                    Ok(payload) => {
                        classify_for_removal(&canonical_root, expected, &id, Ok(&payload.spec))
                    }
                    Err(problem) => {
                        classify_for_removal(&canonical_root, expected, &id, Err(problem))
                    }
                };
                let record = match class {
                    NativeWorkloadClass::Owned(record)
                    | NativeWorkloadClass::OwnedStale { record, .. } => record,
                    NativeWorkloadClass::Foreign { state_root } => {
                        // Only a workload its recorded root still holds can be found again; one
                        // whose directory moved here would be orphaned by removing it.
                        let recorded = Path::new(&state_root).join(&execution_target_id).join(&id);
                        if !recorded.is_dir() {
                            return Err(NativeSiteCleanupError::MovedWorkload {
                                directory,
                                recorded_state_root: state_root,
                            });
                        }
                        warn!(event_kind = "native_site_workload_foreign", workload_id = %id, recorded_state_root = %state_root, "native workload belongs to another state root and is left untouched by the removal");
                        continue;
                    }
                    NativeWorkloadClass::Malformed(source) => {
                        return Err(NativeSiteCleanupError::Untrusted { directory, source });
                    }
                };
                first_owner.get_or_insert_with(|| {
                    (
                        record.site_id.clone(),
                        record.launch.execution_target.managed_node_id.clone(),
                    )
                });
                owned.push(OwnedWorkload {
                    id,
                    execution_target_id: execution_target_id.clone(),
                    target_directory: target_directory.clone(),
                    supervisor: supervisor.clone(),
                    state,
                });
            }
        }
        Ok(owned)
    }

    /// Canonical path of the native state root, or `None` when the directory has none. A root
    /// that resolves outside the directory, through a symbolic link, is refused: the workloads it
    /// leads to are not the directory's.
    fn canonical_root(&self) -> Result<Option<String>, NativeSiteCleanupError> {
        let io_error = |source| NativeSiteCleanupError::Io {
            path: self.state_root.path().to_owned(),
            source,
        };
        let canonical = match fs::canonicalize(self.state_root.path()) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(io_error(source)),
        };
        let site_root = fs::canonicalize(&self.site_root).map_err(io_error)?;
        if canonical != NativeStateRoot::for_site(site_root).path() {
            return Err(NativeSiteCleanupError::RelocatedRoot {
                path: self.state_root.path().to_owned(),
                resolved: canonical,
            });
        }
        match canonical.to_str() {
            Some(root) => Ok(Some(root.to_owned())),
            None => Err(NativeSiteCleanupError::NonUtf8 { path: canonical }),
        }
    }

    /// Lists each execution target directory under the canonical root, by target id. Other
    /// entries hold no workloads and go with the removed directory.
    fn target_directories(
        &self,
        canonical_root: &str,
    ) -> Result<Vec<(String, PathBuf)>, NativeSiteCleanupError> {
        let io_error = |source| NativeSiteCleanupError::Io {
            path: PathBuf::from(canonical_root),
            source,
        };
        let mut targets = Vec::new();
        for entry in fs::read_dir(canonical_root).map_err(io_error)? {
            let entry = entry.map_err(io_error)?;
            let is_directory = entry.file_type().map_err(io_error)?.is_dir();
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if is_directory && valid_id(&name) {
                targets.push((name, entry.path()));
            }
        }
        targets.sort();
        Ok(targets)
    }
}

impl LocatedWorkloadCleanup for NativeSiteCleanup {
    fn runtime_name(&self) -> &'static str {
        NATIVE_RUNTIME_NAME
    }

    fn inventory(&self) -> Pin<Box<dyn Future<Output = anyhow::Result<Vec<String>>> + Send + '_>> {
        Box::pin(async { Ok(NativeSiteCleanup::inventory(self).await?) })
    }

    fn cleanup(&self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async { Ok(NativeSiteCleanup::cleanup(self).await?) })
    }
}

/// Native workload cleanup that cannot proceed; nothing was stopped when classification fails.
#[derive(Debug, thiserror::Error)]
pub enum NativeSiteCleanupError {
    #[error(
        "native workload {} is still being created; retry once its launcher records itself, at most {} seconds after the create started",
        directory.display(),
        INTERRUPTED_CREATE_AGE.as_secs()
    )]
    CreateInProgress { directory: PathBuf },

    #[error(
        "native workload directory {} cannot be trusted, so the removal neither stops nor removes it; stop its processes and remove the directory by hand, then retry",
        directory.display()
    )]
    Untrusted {
        directory: PathBuf,
        #[source]
        source: NativeOwnershipError,
    },

    #[error(
        "native workload directory {} is running for state root {recorded_state_root}, which no longer holds it, so removing it would orphan its processes; move the directory being removed back to that location, or stop the workload's processes by hand, then retry",
        directory.display()
    )]
    MovedWorkload {
        directory: PathBuf,
        recorded_state_root: String,
    },

    #[error(
        "native workload state root {} resolves to {}, outside the directory being removed; remove the link by hand",
        path.display(),
        resolved.display()
    )]
    RelocatedRoot { path: PathBuf, resolved: PathBuf },

    #[error("native workload state root {} is not a UTF-8 path", path.display())]
    NonUtf8 { path: PathBuf },

    #[error("read native workload state at {}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error(transparent)]
    Runtime(#[from] RuntimeBackendError),
}

/// One owned workload whose launcher may still hold a process.
struct OwnedWorkload {
    id: String,
    execution_target_id: String,
    target_directory: PathBuf,
    supervisor: Supervisor,
    /// Operator-facing launcher state, such as `running`.
    state: &'static str,
}

impl OwnedWorkload {
    fn description(&self) -> String {
        format!(
            "workload {} on execution target {} ({})",
            self.id, self.execution_target_id, self.state
        )
    }
}

/// Reads a workload's verified launch payload, or the reason its files cannot be trusted.
async fn read_payload(
    supervisor: &Supervisor,
    id: &str,
) -> Result<Result<LaunchPayload, Inconsistency>, NativeSiteCleanupError> {
    match supervisor.payload(id).await {
        Ok(payload) => Ok(Ok(payload)),
        Err(SupervisorError::Inconsistent(problem)) => Ok(Err(problem)),
        Err(error) => {
            Err(supervisor_error(format!("read native workload {id} launch record"), error).into())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use dotcore_host_process::start_token;
    use dotcore_launcher::workload::Lifetime;

    use super::super::test_support::{NativeTestHost, TEST_EXECUTION_TARGET, TEST_SITE_NODE};
    use super::*;

    #[tokio::test]
    async fn live_owned_workloads_are_listed_then_stopped_and_removed() {
        let site = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("Site directory");
        let host = NativeTestHost::new(site.path(), "site-one");
        let running = host.spawn("deployment-one").await;
        let mut earlier_server = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("earlier server process");
        let pid = earlier_server.id();
        let watch = Lifetime::Watch {
            pid,
            start_token: start_token(pid)
                .unwrap()
                .expect("earlier server start token"),
        };
        let draining = host
            .spawn_recorded("deployment-two", "site-one", watch)
            .await;
        let finished = host.spawn("deployment-three").await;
        host.stop(&finished).await;
        let cleanup = NativeSiteCleanup::new(
            site.path(),
            Some("site-one".to_owned()),
            Some(SiteNodeId::parse(TEST_SITE_NODE).unwrap()),
        );

        let inventory = cleanup.inventory().await.expect("inventory");
        let cleaned = cleanup.cleanup().await;
        earlier_server.kill().unwrap();
        earlier_server.wait().unwrap();

        assert_eq!(
            inventory,
            [
                format!(
                    "workload {} on execution target mac-gpu (running)",
                    running.id
                ),
                format!(
                    "workload {} on execution target mac-gpu (stopping)",
                    draining.id
                ),
            ]
        );
        cleaned.expect("cleanup");
        for workload in [&running, &draining] {
            assert!(!workload.engine_runs(), "{}", workload.id);
            assert!(!workload.directory.exists(), "{}", workload.id);
        }
        assert!(
            finished.directory.exists(),
            "finished workloads go with the removed directory"
        );
        assert_eq!(
            cleanup.inventory().await.expect("inventory after"),
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn a_copied_site_directory_neither_lists_nor_stops_the_original_workloads() {
        let original = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("original site directory");
        let host = NativeTestHost::new(original.path(), "site-one");
        let workload = host.spawn("deployment-one").await;
        let copy = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("copied site directory");
        copy_tree(
            &original.path().join("runtime"),
            &copy.path().join("runtime"),
        );
        let copied = NativeSiteCleanup::new(copy.path(), None, None);

        let inventory = copied.inventory().await.expect("inventory of the copy");
        copied.cleanup().await.expect("cleanup of the copy");

        assert_eq!(inventory, Vec::<String>::new());
        assert!(workload.engine_runs());
        NativeSiteCleanup::new(original.path(), None, None)
            .cleanup()
            .await
            .expect("cleanup of the original");
        assert!(!workload.engine_runs());
    }

    #[tokio::test]
    async fn a_moved_site_directory_refuses_removal_while_its_workloads_run() {
        let root = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("fixture root");
        let original = root.path().join("site");
        let moved = root.path().join("moved");
        let host = NativeTestHost::new(&original, "site-one");
        let workload = host.spawn("deployment-one").await;
        fs::rename(&original, &moved).unwrap();

        let refused = NativeSiteCleanup::new(&moved, None, None).cleanup().await;
        fs::rename(&moved, &original).unwrap();

        let error = refused.expect_err("moved site directory");
        assert!(
            matches!(&error, NativeSiteCleanupError::MovedWorkload { directory, recorded_state_root }
                if *directory == moved.join("runtime/native/mac-gpu").join(&workload.id)
                    && Path::new(recorded_state_root) == original.join("runtime/native")),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("move the directory being removed back to that location")
        );
        assert!(workload.engine_runs());
        NativeSiteCleanup::new(&original, None, None)
            .cleanup()
            .await
            .expect("cleanup at the original location");
        assert!(!workload.engine_runs());
    }

    #[tokio::test]
    async fn contradictory_evidence_under_its_own_root_refuses_removal_and_stops_nothing() {
        let site = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("Site directory");
        let host = NativeTestHost::new(site.path(), "site-one");
        let first = host.spawn("deployment-one").await;
        let second = host
            .spawn_recorded("deployment-two", "site-two", host.watching_this_process())
            .await;
        let node = |id: &str| Some(SiteNodeId::parse(id).unwrap());
        let cases = [
            (
                "Site differs from the configured one",
                NativeSiteCleanup::new(site.path(), Some("site-three".to_owned()), None),
                &first,
                "names site \"site-one\", not this site",
            ),
            (
                "node differs from the bound one",
                NativeSiteCleanup::new(site.path(), Some("site-one".to_owned()), node("node-b")),
                &first,
                "names node \"node-a\", not this node",
            ),
            (
                "workloads of one root disagree on their site",
                NativeSiteCleanup::new(site.path(), None, None),
                &second,
                "names site \"site-two\", not this site",
            ),
        ];

        for (name, cleanup, refused, reason) in cases {
            let error = anyhow::Error::new(cleanup.cleanup().await.expect_err(name));

            assert_eq!(
                format!("{error:#}"),
                format!(
                    "native workload directory {} cannot be trusted, so the removal neither stops nor removes it; stop its processes and remove the directory by hand, then retry: native workload owner record {reason}",
                    refused.directory.display()
                ),
                "{name}"
            );
            assert!(first.engine_runs() && second.engine_runs(), "{name}");
        }

        host.stop(&first).await;
        NativeSiteCleanup::new(site.path(), Some("site-two".to_owned()), None)
            .cleanup()
            .await
            .expect("cleanup once only one site remains live");
        assert!(!second.engine_runs());
    }

    #[tokio::test]
    async fn a_create_in_progress_refuses_removal() {
        let site = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("Site directory");
        let directory = site
            .path()
            .join("runtime/native")
            .join(TEST_EXECUTION_TARGET)
            .join("deployment-one_1");
        fs::create_dir_all(&directory).unwrap();

        let error = NativeSiteCleanup::new(site.path(), None, None)
            .inventory()
            .await
            .expect_err("create in progress");

        assert_eq!(
            error.to_string(),
            format!(
                "native workload {} is still being created; retry once its launcher records itself, at most 60 seconds after the create started",
                directory.display()
            )
        );
    }

    #[tokio::test]
    async fn a_state_root_linked_outside_the_site_directory_is_refused() {
        let site = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("Site directory");
        let elsewhere = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("other directory");
        fs::create_dir(site.path().join("runtime")).unwrap();
        symlink(elsewhere.path(), site.path().join("runtime/native")).unwrap();

        let error = NativeSiteCleanup::new(site.path(), None, None)
            .cleanup()
            .await
            .expect_err("linked root");

        assert!(
            matches!(&error, NativeSiteCleanupError::RelocatedRoot { resolved, .. } if resolved == elsewhere.path()),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn a_site_without_native_state_has_nothing_to_clean() {
        let site = tempfile::tempdir_in(dotcore_domain::test_support::canonical_temp_dir())
            .expect("Site directory");
        let cleanup = NativeSiteCleanup::new(site.path(), None, None);

        assert_eq!(
            cleanup.inventory().await.expect("inventory"),
            Vec::<String>::new()
        );
        cleanup.cleanup().await.expect("cleanup");
        assert!(!site.path().join("runtime").exists());
    }

    fn copy_tree(source: &Path, destination: &Path) {
        fs::create_dir_all(destination).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let target = destination.join(entry.file_name());
            if entry.file_type().unwrap().is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                fs::copy(entry.path(), &target).unwrap();
            }
        }
    }
}
