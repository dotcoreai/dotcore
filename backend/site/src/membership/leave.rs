//! Lets a removed machine leave its site: stops and removes the runtime workloads
//! that belong to this node, then deletes the joined node folder.
//!
//! Docker and simulated cleanup acts only on resources labelled with the site and one of the
//! folder's own execution targets, so a manager or another node sharing the same Docker daemon
//! keeps its workloads; native workloads are found by their location in the folder and must
//! record this site and node. The operation holds the node custody lock, so it refuses while the
//! node's server runs, and it never contacts the manager.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use dotcore_domain::{SiteNodeId, SiteNodeIdError};
use dotcore_proto::dotcore::site::v1::{
    ExecutionTarget, application_runtime_binding, execution_target, model_runtime_binding,
};
use dotcore_runtime::{
    RuntimeBindingKind, SiteRuntimeCleanup, SiteRuntimeCleanupReport, SiteRuntimeCleanups,
    SiteRuntimeInventory, SiteRuntimeResetConfigurationError,
};
use tracing::{info, warn};

use crate::{
    SITE_FILE,
    node::folder::{
        NodeFolderValidationError, WORKER_MANIFEST_FILE, WorkerNodeCustody, WorkerNodeCustodyError,
        read_bootstrap,
    },
    reset::{
        runtime_removal::{RuntimeRemoval, RuntimeRemovalError},
        transaction::SitePersistenceCleanup,
    },
};

/// A joined node folder opened for leaving its site, with custody of the folder held.
/// It names what leaving will remove so the operator can confirm before anything changes.
#[derive(Debug)]
pub struct NodeLeave {
    node_root: PathBuf,
    site_id: String,
    node_id: String,
    execution_target_ids: BTreeSet<String>,
    binding_kinds: BTreeSet<RuntimeBindingKind>,

    /// Runtime-specific cleanups, selected by the binding kinds of the node's execution targets.
    cleanups: Arc<dyn SiteRuntimeCleanups>,

    /// Runtime cleanup still to run, or `None` when an earlier leave already finished it.
    runtime_removal: Option<RuntimeRemoval>,
    _custody: Option<WorkerNodeCustody>,
}

impl NodeLeave {
    /// Opens a joined node folder and takes its custody lock.
    /// Refuses a manager site folder, a folder that is not a joined node folder, a node whose
    /// server is running, and a node with an execution target that has no runtime binding.
    ///
    /// A folder whose earlier leave finished runtime cleanup but was interrupted while deleting
    /// the folder opens in a resumed state that only finishes the deletion.
    pub fn open(
        node_root: &Path,
        cleanups: Arc<dyn SiteRuntimeCleanups>,
    ) -> Result<Self, NodeLeaveError> {
        if let Some(resumed) = Self::resume(node_root, &cleanups)? {
            return Ok(resumed);
        }
        require_joined_node_folder(node_root)?;
        let custody = WorkerNodeCustody::try_acquire(node_root).map_err(|source| match source {
            WorkerNodeCustodyError::AlreadyHeld => NodeLeaveError::NodeRunning {
                node_root: node_root.to_owned(),
            },
            source => NodeLeaveError::Custody {
                node_root: node_root.to_owned(),
                source,
            },
        })?;
        let bootstrap =
            read_bootstrap(node_root).map_err(|source| NodeLeaveError::InvalidNodeFolder {
                node_root: node_root.to_owned(),
                source,
            })?;

        let mut binding_kinds = BTreeSet::new();
        for target in &bootstrap.realization_targets {
            let Some(kind) = cleanup_binding_kind(target) else {
                return Err(NodeLeaveError::UnsupportedRuntime {
                    execution_target_id: target.execution_target_id.clone(),
                });
            };
            binding_kinds.insert(kind);
        }
        let execution_target_ids = bootstrap
            .realization_targets
            .iter()
            .map(|target| target.execution_target_id.clone())
            .collect::<BTreeSet<_>>();
        // Native cleanup must match workloads to this exact SiteNode; it never falls back to
        // accepting whatever node the workloads record.
        let site_node_id = SiteNodeId::parse(&bootstrap.worker_node_id).map_err(|source| {
            NodeLeaveError::InvalidNodeId {
                node_root: node_root.to_owned(),
                source,
            }
        })?;
        let runtime_removal = RuntimeRemoval::node_folder(
            cleanups.as_ref(),
            node_root,
            &bootstrap.site_id,
            site_node_id,
            execution_target_ids.clone(),
        );

        Ok(Self {
            node_root: node_root.to_owned(),
            site_id: bootstrap.site_id,
            node_id: bootstrap.worker_node_id,
            execution_target_ids,
            binding_kinds,
            cleanups,
            runtime_removal: Some(runtime_removal),
            _custody: Some(custody),
        })
    }

    fn resume(
        node_root: &Path,
        cleanups: &Arc<dyn SiteRuntimeCleanups>,
    ) -> Result<Option<Self>, NodeLeaveError> {
        if exists(&node_root.join(SITE_FILE))? {
            return Err(NodeLeaveError::ManagerFolder {
                node_root: node_root.to_owned(),
            });
        }
        if exists(&node_root.join(WORKER_MANIFEST_FILE))? {
            return Ok(None);
        }
        let marker = pending_marker(node_root);
        let contents = match fs::read_to_string(&marker) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(NodeLeaveError::Inspection {
                    path: marker,
                    source,
                });
            }
        };
        let Some((site_id, node_id)) = contents.trim_end().split_once('\n') else {
            return Ok(None);
        };

        let custody = if exists(node_root)? {
            Some(
                WorkerNodeCustody::try_acquire(node_root).map_err(|source| match source {
                    WorkerNodeCustodyError::AlreadyHeld => NodeLeaveError::NodeRunning {
                        node_root: node_root.to_owned(),
                    },
                    source => NodeLeaveError::Custody {
                        node_root: node_root.to_owned(),
                        source,
                    },
                })?,
            )
        } else {
            None
        };
        Ok(Some(Self {
            node_root: node_root.to_owned(),
            site_id: site_id.to_owned(),
            node_id: node_id.to_owned(),
            execution_target_ids: BTreeSet::new(),
            binding_kinds: BTreeSet::new(),
            cleanups: Arc::clone(cleanups),
            runtime_removal: None,
            _custody: custody,
        }))
    }

    /// Returns the identity of the site this node is leaving.
    pub fn site_id(&self) -> &str {
        &self.site_id
    }

    /// Returns the identity of the leaving SiteNode.
    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    /// Returns the execution targets whose runtime resources leaving removes.
    pub fn execution_target_ids(&self) -> &BTreeSet<String> {
        &self.execution_target_ids
    }

    /// Returns the node folder that leaving deletes.
    pub fn node_root(&self) -> &Path {
        &self.node_root
    }

    /// Selects runtime cleanup from the runtime bindings of this node's execution targets.
    pub fn runtime_cleanup(
        &self,
    ) -> Result<Box<dyn SiteRuntimeCleanup>, SiteRuntimeResetConfigurationError> {
        self.cleanups.site_cleanup(&self.binding_kinds)
    }

    /// Lists the runtime resources of this node's execution targets, then the native workloads in
    /// the node folder, without changing anything. A resumed leave lists nothing.
    pub async fn inventory(
        &self,
        runtime: &dyn SiteRuntimeCleanup,
    ) -> Result<SiteRuntimeInventory, NodeLeaveError> {
        match &self.runtime_removal {
            Some(removal) => removal
                .inventory(runtime)
                .await
                .map_err(|error| self.runtime_removal_error(error)),
            None => Ok(SiteRuntimeInventory::default()),
        }
    }

    /// Removes the runtime resources of this node's execution targets and the native workloads in
    /// the node folder, then deletes the folder. A folder unsafe to delete and untrusted native
    /// evidence both refuse before any runtime changes. A native stop or removal failing later
    /// can leave the other runtimes already cleaned; the folder is untouched when any cleanup
    /// fails, so the leave can be retried.
    pub async fn leave(
        self,
        runtime: &dyn SiteRuntimeCleanup,
        persistence: &dyn SitePersistenceCleanup,
    ) -> Result<SiteRuntimeCleanupReport, NodeLeaveError> {
        info!(event_kind = "node_leave_requested", site_id = %self.site_id, node_id = %self.node_id, node_root = %self.node_root.display(), "node leave requested");
        let marker = pending_marker(&self.node_root);
        let report = if let Some(removal) = &self.runtime_removal {
            persistence
                .preflight(&self.node_root)
                .await
                .map_err(|source| NodeLeaveError::UnsafeFolder {
                    node_root: self.node_root.clone(),
                    source,
                })?;
            let report = removal
                .cleanup(runtime)
                .await
                .map_err(|error| self.runtime_removal_error(error))?;
            // The marker sits beside the folder so it outlives an interrupted folder deletion,
            // which can remove the manifest before the rest.
            fs::write(&marker, format!("{}\n{}\n", self.site_id, self.node_id)).map_err(
                |source| NodeLeaveError::FolderCleanup {
                    node_root: self.node_root.clone(),
                    source: anyhow::Error::new(source)
                        .context("record the pending folder deletion"),
                },
            )?;
            report
        } else {
            SiteRuntimeCleanupReport::default()
        };
        persistence
            .cleanup(&self.node_root)
            .await
            .map_err(|source| NodeLeaveError::FolderCleanup {
                node_root: self.node_root.clone(),
                source,
            })?;
        if let Err(error) = fs::remove_file(&marker)
            && error.kind() != io::ErrorKind::NotFound
        {
            warn!(event_kind = "node_leave_marker_left", marker = %marker.display(), error = %error, "could not remove the pending deletion marker");
        }
        info!(event_kind = "node_leave_completed", site_id = %self.site_id, node_id = %self.node_id, "node left its site");
        Ok(report)
    }

    fn runtime_removal_error(&self, error: RuntimeRemovalError) -> NodeLeaveError {
        match error {
            RuntimeRemovalError::Native(source) => NodeLeaveError::NativeCleanup {
                node_root: self.node_root.clone(),
                source,
            },
            RuntimeRemovalError::Runtime(source) => NodeLeaveError::RuntimeCleanup {
                node_id: self.node_id.clone(),
                source,
            },
        }
    }
}

/// A node could not leave its site.
#[derive(Debug, thiserror::Error)]
pub enum NodeLeaveError {
    /// The folder holds a manager site configuration.
    #[error(
        "{node_root} holds a site (site.yaml), not a joined node; remove it by deleting the site"
    )]
    ManagerFolder {
        /// Folder that was opened.
        node_root: PathBuf,
    },

    /// The folder is empty, absent, or holds something other than a joined node.
    #[error("{node_root} is not a joined node folder (no worker.yaml)")]
    NotJoinedNodeFolder {
        /// Folder that was opened.
        node_root: PathBuf,
    },

    /// Another process holds the node custody lock.
    #[error("the node server for {node_root} is running; stop it before leaving the site")]
    NodeRunning {
        /// Folder whose custody lock is held.
        node_root: PathBuf,
    },

    /// The custody lock could not be taken for a reason other than another holder.
    #[error("take custody of the node folder {node_root}: {source}")]
    Custody {
        /// Folder that was opened.
        node_root: PathBuf,

        /// Underlying custody failure.
        #[source]
        source: WorkerNodeCustodyError,
    },

    /// The node manifest could not be read or violates its contract.
    #[error("read the node manifest in {node_root}")]
    InvalidNodeFolder {
        /// Folder that was opened.
        node_root: PathBuf,

        /// Underlying manifest failure.
        #[source]
        source: NodeFolderValidationError,
    },

    /// The node manifest names a SiteNode id that native workload cleanup cannot match.
    #[error("the node manifest in {node_root} names an invalid node id")]
    InvalidNodeId {
        /// Folder that was opened.
        node_root: PathBuf,

        /// Underlying identity failure.
        #[source]
        source: SiteNodeIdError,
    },

    /// An execution target has no runtime binding or adapter, so its workloads cannot be found
    /// and the folder cannot be deleted safely.
    #[error(
        "execution target {execution_target_id} has no runtime binding whose workloads leaving can clean up; leaving refuses so no workload is orphaned"
    )]
    UnsupportedRuntime {
        /// Execution target with the unsupported binding.
        execution_target_id: String,
    },

    /// The folder could not be inspected.
    #[error("inspect {path}: {source}")]
    Inspection {
        /// Path that could not be inspected.
        path: PathBuf,

        /// Underlying filesystem failure.
        #[source]
        source: io::Error,
    },

    /// Runtime cleanup failed or found resources it cannot attribute safely.
    #[error("clean up the runtime resources of node {node_id}")]
    RuntimeCleanup {
        /// Leaving node.
        node_id: String,

        /// Underlying runtime failure.
        #[source]
        source: anyhow::Error,
    },

    /// Native workloads in the node folder could not be classified, stopped, or removed, so the
    /// folder is kept.
    #[error("clean up the native workloads in {node_root}")]
    NativeCleanup {
        /// Folder whose native workloads remain.
        node_root: PathBuf,

        /// Underlying native cleanup failure.
        #[source]
        source: anyhow::Error,
    },

    /// The node folder cannot be deleted safely, so leaving stopped nothing.
    #[error("the node folder {node_root} cannot be deleted safely, so no workload was stopped")]
    UnsafeFolder {
        /// Folder that was checked.
        node_root: PathBuf,

        /// Underlying safety check failure.
        #[source]
        source: anyhow::Error,
    },

    /// The node folder could not be deleted after runtime cleanup.
    #[error("delete the node folder {node_root}")]
    FolderCleanup {
        /// Folder that could not be deleted.
        node_root: PathBuf,

        /// Underlying deletion failure.
        #[source]
        source: anyhow::Error,
    },
}

/// Returns the file beside the node folder that records a finished runtime cleanup whose folder
/// deletion is still pending.
fn pending_marker(node_root: &Path) -> PathBuf {
    let name = node_root.file_name().unwrap_or_default().to_string_lossy();
    node_root.with_file_name(format!("{name}.leave-pending"))
}

fn require_joined_node_folder(node_root: &Path) -> Result<(), NodeLeaveError> {
    if exists(&node_root.join(SITE_FILE))? {
        return Err(NodeLeaveError::ManagerFolder {
            node_root: node_root.to_owned(),
        });
    }
    let manifest = node_root.join(WORKER_MANIFEST_FILE);
    match fs::symlink_metadata(&manifest) {
        Ok(metadata) if metadata.is_file() => Ok(()),
        Ok(_) => Err(NodeLeaveError::NotJoinedNodeFolder {
            node_root: node_root.to_owned(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(NodeLeaveError::NotJoinedNodeFolder {
                node_root: node_root.to_owned(),
            })
        }
        Err(source) => Err(NodeLeaveError::Inspection {
            path: manifest,
            source,
        }),
    }
}

fn exists(path: &Path) -> Result<bool, NodeLeaveError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(NodeLeaveError::Inspection {
            path: path.to_owned(),
            source,
        }),
    }
}

/// Returns the runtime binding kind whose resources leaving removes for this target, or `None`
/// when the target has no binding or adapter. Every adapter is listed so a new runtime adapter
/// must decide its cleanup here.
fn cleanup_binding_kind(target: &ExecutionTarget) -> Option<RuntimeBindingKind> {
    match target.runtime_binding.as_ref()? {
        execution_target::RuntimeBinding::ModelRuntime(binding) => {
            match binding.adapter.as_ref()? {
                model_runtime_binding::Adapter::SimulatedRuntime(_) => {
                    Some(RuntimeBindingKind::Simulated)
                }
                model_runtime_binding::Adapter::DockerRuntime(_) => {
                    Some(RuntimeBindingKind::Docker)
                }
                // Native workloads are cleaned by their location in the node folder on every leave;
                // selecting the backend only keeps the binding accepted.
                model_runtime_binding::Adapter::NativeRuntime(_) => {
                    Some(RuntimeBindingKind::Native)
                }
            }
        }
        execution_target::RuntimeBinding::ApplicationRuntime(binding) => {
            match binding.adapter.as_ref()? {
                application_runtime_binding::Adapter::DockerRuntime(_) => {
                    Some(RuntimeBindingKind::Docker)
                }
                application_runtime_binding::Adapter::SimulatedRuntime(_) => {
                    Some(RuntimeBindingKind::Simulated)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
        time::SystemTime,
    };

    use dotcore_domain::ExecutionPlatform;
    use dotcore_domain::test_support::canonical_tempdir;
    use dotcore_host_process::start_token;
    use dotcore_launcher::{supervisor::INTERRUPTED_CREATE_AGE, workload::Lifetime};
    use dotcore_proto::dotcore::site::v1::{ModelRuntimeBinding, NativeWorkloadLifetime};
    use dotcore_runtime::{SiteRuntimeResource, SiteRuntimeScope};
    use dotcore_runtime_backends::native::{
        NativeSiteCleanup,
        test_support::{NativeTestHost, TEST_EXECUTION_TARGET, TEST_SITE_NODE},
    };

    use super::*;
    use crate::{
        initial::{
            inputs::{
                ApplicationTargetChoice, ModelTargetChoice, NodeTargetChoices,
                build_execution_targets,
            },
            request::InitialApplicationRuntime,
        },
        membership::join::test_fixtures::{initialized_site, worker_node, worker_targets},
        node::{
            folder::{CertificateFingerprint, NodeFolderBootstrap, NodeFolderManager},
            observation_configuration::node_observation_configuration,
        },
        reset::persistence::SitePersistenceReset,
    };

    #[test]
    fn open_reports_the_site_node_and_exact_targets_to_remove() {
        let fixture = NodeFolderFixture::new();

        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");

        assert_eq!(leave.site_id(), fixture.site_id);
        assert_eq!(leave.node_id(), "node-worker");
        assert_eq!(
            leave.execution_target_ids().iter().collect::<Vec<_>>(),
            ["node-worker-application-target", "node-worker-model-target"]
        );
        assert_eq!(leave.node_root(), fixture.root);
    }

    #[test]
    fn a_held_custody_lock_refuses_with_the_node_running_message() {
        let fixture = NodeFolderFixture::new();
        let _running = WorkerNodeCustody::try_acquire(&fixture.root).expect("server custody");

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect_err("running node refused");

        assert_eq!(
            error.to_string(),
            format!(
                "the node server for {} is running; stop it before leaving the site",
                fixture.root.display()
            )
        );
    }

    #[test]
    fn a_manager_folder_is_refused_before_any_lock_is_created() {
        let fixture = NodeFolderFixture::new();
        fs::write(fixture.root.join(SITE_FILE), "site").expect("site.yaml");
        let lock = fixture
            .root
            .join(crate::node::folder::WORKER_CUSTODY_LOCK_FILE);
        fs::remove_file(&lock).expect("remove custody lock");

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect_err("manager folder refused");

        assert_eq!(
            error.to_string(),
            format!(
                "{} holds a site (site.yaml), not a joined node; remove it by deleting the site",
                fixture.root.display()
            )
        );
        assert!(!lock.exists());
    }

    #[test]
    fn empty_and_absent_folders_are_refused_without_creating_anything() {
        let directory = canonical_tempdir().expect("temporary directory");
        let empty = directory.path().join("empty");
        fs::create_dir(&empty).expect("empty folder");
        let absent = directory.path().join("absent");

        for folder in [&empty, &absent] {
            let error = NodeLeave::open(folder, crate::test_support::site_runtime_cleanups())
                .expect_err("not a node folder");

            assert_eq!(
                error.to_string(),
                format!(
                    "{} is not a joined node folder (no worker.yaml)",
                    folder.display()
                )
            );
        }
        assert_eq!(fs::read_dir(&empty).expect("read empty").count(), 0);
        assert!(!absent.exists());
    }

    #[test]
    fn a_target_without_scoped_cleanup_is_refused() {
        let fixture = NodeFolderFixture::new();
        fixture.rewrite_targets(|targets| {
            targets[0].runtime_binding = Some(execution_target::RuntimeBinding::ModelRuntime(
                ModelRuntimeBinding { adapter: None },
            ));
        });

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect_err("unsupported binding refused");

        assert!(
            matches!(
                &error,
                NodeLeaveError::UnsupportedRuntime { execution_target_id }
                    if execution_target_id == "node-worker-model-target"
            ),
            "{error}"
        );
        assert_eq!(
            error.to_string(),
            "execution target node-worker-model-target has no runtime binding whose workloads leaving can clean up; leaving refuses so no workload is orphaned"
        );
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
    }

    #[test]
    fn an_invalid_node_id_is_refused_before_any_runtime_cleanup_is_selected() {
        let fixture = NodeFolderFixture::new();
        let path = fixture.root.join(WORKER_MANIFEST_FILE);
        let manifest = fs::read_to_string(&path).expect("read manifest");
        fs::write(
            &path,
            manifest.replace("workerNodeId: node-worker", "workerNodeId: Node_Worker"),
        )
        .expect("rewrite manifest");

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect_err("invalid node id refused");

        assert!(
            matches!(&error, NodeLeaveError::InvalidNodeFolder { .. }),
            "{error:?}"
        );
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
    }

    #[tokio::test]
    async fn leave_cleans_only_this_nodes_targets_and_then_deletes_the_folder() {
        let fixture = NodeFolderFixture::new();
        let runtime = RecordingRuntime::default();
        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");

        let inventory = leave.inventory(&runtime).await.expect("inventory");
        assert_eq!(
            inventory
                .resources
                .iter()
                .map(|resource| resource.description.as_str())
                .collect::<Vec<_>>(),
            ["worker container"]
        );
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());

        leave
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect("leave");

        let scope = SiteRuntimeScope::ExecutionTargets(
            [
                "node-worker-application-target".to_owned(),
                "node-worker-model-target".to_owned(),
            ]
            .into(),
        );
        assert_eq!(
            *runtime.calls.lock().expect("calls"),
            [
                (
                    "inventory".to_owned(),
                    fixture.site_id.clone(),
                    scope.clone()
                ),
                ("cleanup".to_owned(), fixture.site_id.clone(), scope),
            ]
        );
        assert!(!fixture.root.exists());
    }

    #[tokio::test]
    async fn a_failed_runtime_cleanup_keeps_the_folder_for_a_retry() {
        let fixture = NodeFolderFixture::new();
        let runtime = RecordingRuntime {
            fail_cleanup: true,
            ..RecordingRuntime::default()
        };
        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");

        let error = leave
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect_err("cleanup failure");

        assert_eq!(
            format!("{error:#}"),
            "clean up the runtime resources of node node-worker"
        );
        assert_eq!(
            std::error::Error::source(&error)
                .expect("source")
                .to_string(),
            "injected cleanup failure"
        );
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
    }

    #[tokio::test]
    async fn an_interrupted_folder_deletion_is_finished_by_a_retry_without_new_runtime_cleanup() {
        let fixture = NodeFolderFixture::new();
        let runtime = RecordingRuntime::default();
        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");

        let error = leave
            .leave(&runtime, &InterruptedDeletion)
            .await
            .expect_err("interrupted deletion");
        assert_eq!(
            format!("{error:#}"),
            format!("delete the node folder {}", fixture.root.display())
        );
        assert!(!fixture.root.join(WORKER_MANIFEST_FILE).exists());
        assert!(fixture.root.is_dir());
        // Native state that a fresh leave would refuse; the resumed leave never inspects it.
        fs::create_dir_all(fixture.native_target().join("deployment-one_1"))
            .expect("native create in progress");

        let retry = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("retry opens the leftovers");
        assert_eq!(retry.site_id(), fixture.site_id);
        assert_eq!(retry.node_id(), "node-worker");
        let inventory = retry.inventory(&runtime).await.expect("inventory");
        assert!(inventory.resources.is_empty());
        retry
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect("retry finishes the deletion");

        assert!(!fixture.root.exists());
        assert!(!pending_marker(&fixture.root).exists());
        // Only the first attempt touched the runtime.
        assert_eq!(runtime.calls.lock().expect("calls").len(), 1);
        assert!(matches!(
            NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups()),
            Err(NodeLeaveError::NotJoinedNodeFolder { .. })
        ));
    }

    #[tokio::test]
    async fn native_and_docker_workloads_are_listed_then_both_cleaned_before_the_folder_is_deleted()
    {
        let fixture =
            NodeFolderFixture::with_targets(TEST_SITE_NODE, native_targets(TEST_SITE_NODE));
        let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
        let running = host.spawn("deployment-one").await;
        let mut earlier_server = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("earlier server process");
        let pid = earlier_server.id();
        let watch = Lifetime::Watch {
            pid,
            start_token: start_token(pid)
                .expect("read start token")
                .expect("earlier server start token"),
        };
        let draining = host
            .spawn_recorded("deployment-two", &fixture.site_id, watch)
            .await;
        let runtime = RecordingRuntime::default();
        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");
        leave
            .runtime_cleanup()
            .expect("native and Docker bindings select a cleanup");

        let inventory = leave.inventory(&runtime).await.expect("inventory");
        let left = leave
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await;
        earlier_server.kill().expect("stop earlier server");
        earlier_server.wait().expect("reap earlier server");

        assert_eq!(
            inventory
                .resources
                .iter()
                .map(|resource| (resource.runtime, resource.description.clone()))
                .collect::<Vec<_>>(),
            [
                ("Test Runtime", "worker container".to_owned()),
                (
                    "Native runtime",
                    format!(
                        "workload {} on execution target mac-gpu (running)",
                        running.id
                    )
                ),
                (
                    "Native runtime",
                    format!(
                        "workload {} on execution target mac-gpu (stopping)",
                        draining.id
                    )
                ),
            ]
        );
        left.expect("leave");
        let scope = SiteRuntimeScope::ExecutionTargets(
            [
                format!("{TEST_SITE_NODE}-application-target"),
                format!("{TEST_SITE_NODE}-model-target"),
            ]
            .into(),
        );
        assert_eq!(
            *runtime.calls.lock().expect("calls"),
            [
                (
                    "inventory".to_owned(),
                    fixture.site_id.clone(),
                    scope.clone()
                ),
                ("cleanup".to_owned(), fixture.site_id.clone(), scope),
            ]
        );
        assert!(!running.engine_runs());
        assert!(!draining.engine_runs());
        assert!(!fixture.root.exists());
        assert!(!pending_marker(&fixture.root).exists());
    }

    #[tokio::test]
    async fn native_state_under_a_folder_bound_to_other_runtimes_is_still_cleaned() {
        let fixture =
            NodeFolderFixture::with_targets(TEST_SITE_NODE, worker_targets(TEST_SITE_NODE));
        let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
        let workload = host.spawn("deployment-one").await;
        let runtime = RecordingRuntime::default();
        let leave = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens");

        let inventory = leave.inventory(&runtime).await.expect("inventory");
        leave
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect("leave");

        assert_eq!(
            inventory
                .resources
                .iter()
                .map(|resource| (resource.runtime, resource.description.clone()))
                .collect::<Vec<_>>(),
            [
                ("Test Runtime", "worker container".to_owned()),
                (
                    "Native runtime",
                    format!(
                        "workload {} on execution target mac-gpu (running)",
                        workload.id
                    )
                ),
            ]
        );
        assert!(!workload.engine_runs());
        assert!(!fixture.root.exists());
    }

    #[tokio::test]
    async fn untrusted_native_evidence_refuses_before_any_runtime_is_contacted() {
        for (name, node_id, recorded_site, reason) in [
            (
                "workload of another site",
                TEST_SITE_NODE,
                Some("other-site"),
                "names site \"other-site\", not this site",
            ),
            (
                "workload of another node",
                "node-worker",
                None,
                "names node \"node-a\", not this node",
            ),
        ] {
            let fixture = NodeFolderFixture::with_targets(node_id, native_targets(node_id));
            let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
            let workload = match recorded_site {
                Some(site_id) => {
                    host.spawn_recorded("deployment-one", site_id, host.watching_this_process())
                        .await
                }
                None => host.spawn("deployment-one").await,
            };
            let runtime = RecordingRuntime::default();
            let leave =
                NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
                    .expect(name);

            let inventory = leave.inventory(&runtime).await;
            let left = leave
                .leave(
                    &runtime,
                    &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
                )
                .await;

            for error in [inventory.expect_err(name), left.expect_err(name)] {
                assert!(
                    matches!(&error, NodeLeaveError::NativeCleanup { node_root, .. }
                        if *node_root == fixture.root),
                    "{name}: {error:?}"
                );
                let chain = format!("{:#}", anyhow::Error::new(error));
                assert!(chain.contains(reason), "{name}: {chain}");
            }
            assert!(runtime.calls.lock().expect("calls").is_empty(), "{name}");
            assert!(workload.engine_runs(), "{name}");
            assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file(), "{name}");
            assert!(!pending_marker(&fixture.root).exists(), "{name}");
            NativeSiteCleanup::new(&fixture.root, None, None)
                .cleanup()
                .await
                .expect("stop the refused workload");
        }
    }

    #[tokio::test]
    async fn a_failure_after_runtime_cleanup_writes_no_marker_and_a_rerun_completes() {
        let fixture =
            NodeFolderFixture::with_targets(TEST_SITE_NODE, native_targets(TEST_SITE_NODE));
        let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
        let workload = host.spawn("deployment-one").await;
        // A create that starts while other runtimes are cleaned makes the native cleanup refuse.
        let create = fixture.native_target().join("deployment-two_1");
        let runtime = RecordingRuntime {
            cleanup_creates: Mutex::new(Some(create.clone())),
            ..RecordingRuntime::default()
        };

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens")
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect_err("native cleanup refused");

        assert!(
            matches!(&error, NodeLeaveError::NativeCleanup { .. }),
            "{error:?}"
        );
        assert_eq!(runtime.calls.lock().expect("calls").len(), 1);
        assert!(workload.engine_runs());
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
        assert!(!pending_marker(&fixture.root).exists());

        fs::remove_dir(&create).expect("the create is gone");
        NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("rerun opens")
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect("rerun completes");

        assert_eq!(
            runtime
                .calls
                .lock()
                .expect("calls")
                .iter()
                .map(|(call, _, _)| call.as_str())
                .collect::<Vec<_>>(),
            ["cleanup", "cleanup"]
        );
        assert!(!workload.engine_runs());
        assert!(!fixture.root.exists());
    }

    #[tokio::test]
    async fn a_native_cleanup_failing_partway_keeps_the_folder_and_a_rerun_stops_the_rest() {
        let fixture =
            NodeFolderFixture::with_targets(TEST_SITE_NODE, native_targets(TEST_SITE_NODE));
        let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
        let mut workloads = [
            host.spawn("deployment-one").await,
            host.spawn("deployment-two").await,
        ];
        workloads.sort_by(|left, right| left.id.cmp(&right.id));
        // A file where the launcher stages creates cannot be removed as a directory, so removing
        // the first workload fails once its process has stopped and its directory is gone.
        let leftover = fixture
            .native_target()
            .join(format!(".create-{}", workloads[0].id));
        fs::write(&leftover, "").expect("staging obstacle");
        let runtime = RecordingRuntime::default();

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens")
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect_err("partial native cleanup");

        assert!(
            matches!(&error, NodeLeaveError::NativeCleanup { .. }),
            "{error:?}"
        );
        assert!(!workloads[0].engine_runs());
        assert!(!workloads[0].directory.exists());
        assert!(workloads[1].engine_runs());
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
        assert!(!pending_marker(&fixture.root).exists());

        // The leftover staging entry reads as a create still in progress, so a rerun refuses
        // with nothing stopped until it is old enough to count as an interrupted create.
        let refused = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("rerun opens")
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect_err("young leftover refuses the rerun");
        let chain = format!("{:#}", anyhow::Error::new(refused));
        assert!(
            chain.contains(&format!(
                "native workload {} is still being created",
                fixture.native_target().join(&workloads[0].id).display()
            )),
            "{chain}"
        );
        assert!(workloads[1].engine_runs());

        fs::File::options()
            .write(true)
            .open(&leftover)
            .expect("open leftover")
            .set_modified(SystemTime::now() - 2 * INTERRUPTED_CREATE_AGE)
            .expect("age leftover");
        NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("second rerun opens")
            .leave(
                &runtime,
                &SitePersistenceReset::new(crate::test_support::site_runtime_cleanups()),
            )
            .await
            .expect("rerun completes once the leftover counts as interrupted");

        assert!(!workloads[1].engine_runs());
        assert!(!fixture.root.exists());
    }

    #[tokio::test]
    async fn a_folder_unsafe_to_delete_refuses_before_any_workload_is_stopped() {
        let fixture =
            NodeFolderFixture::with_targets(TEST_SITE_NODE, native_targets(TEST_SITE_NODE));
        let host = NativeTestHost::new(&fixture.root, &fixture.site_id);
        let workload = host.spawn("deployment-one").await;
        let runtime = RecordingRuntime::default();

        let error = NodeLeave::open(&fixture.root, crate::test_support::site_runtime_cleanups())
            .expect("joined folder opens")
            .leave(&runtime, &UnsafeFolder)
            .await
            .expect_err("unsafe folder refused");

        assert!(
            matches!(&error, NodeLeaveError::UnsafeFolder { node_root, .. }
                if *node_root == fixture.root),
            "{error:?}"
        );
        assert_eq!(
            format!("{:#}", anyhow::Error::new(error)),
            format!(
                "the node folder {root} cannot be deleted safely, so no workload was stopped: injected unsafe folder",
                root = fixture.root.display()
            )
        );
        assert!(runtime.calls.lock().expect("calls").is_empty());
        assert!(workload.engine_runs());
        assert!(fixture.root.join(WORKER_MANIFEST_FILE).is_file());
        assert!(!pending_marker(&fixture.root).exists());
        host.stop(&workload).await;
    }

    /// Removes the manifest and then fails, as a folder deletion interrupted midway would.
    struct InterruptedDeletion;

    impl SitePersistenceCleanup for InterruptedDeletion {
        fn preflight<'a>(
            &'a self,
            _node_root: &'a Path,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(std::future::ready(Ok(())))
        }

        fn cleanup<'a>(
            &'a self,
            node_root: &'a Path,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            let removed = fs::remove_file(node_root.join(WORKER_MANIFEST_FILE));
            Box::pin(std::future::ready(
                removed
                    .map_err(anyhow::Error::from)
                    .and_then(|()| Err(anyhow::anyhow!("interrupted"))),
            ))
        }
    }

    /// Refuses every folder as unsafe to delete, and must never be asked to delete one.
    struct UnsafeFolder;

    impl SitePersistenceCleanup for UnsafeFolder {
        fn preflight<'a>(
            &'a self,
            _node_root: &'a Path,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            Box::pin(std::future::ready(Err(anyhow::anyhow!(
                "injected unsafe folder"
            ))))
        }

        fn cleanup<'a>(
            &'a self,
            _node_root: &'a Path,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
            panic!("an unsafe folder must not be deleted")
        }
    }

    struct NodeFolderFixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        site_id: String,
    }

    impl NodeFolderFixture {
        fn new() -> Self {
            Self::with_targets("node-worker", worker_targets("node-worker"))
        }

        /// Joins `node_id` with `targets` as its execution targets.
        fn with_targets(node_id: &str, targets: Vec<ExecutionTarget>) -> Self {
            let directory = canonical_tempdir().expect("temporary directory");
            let root = directory.path().join("node");
            fs::create_dir(&root).expect("node folder");

            let mut site = initialized_site().site;
            site.nodes.push(worker_node(node_id, "worker", "10.0.0.7"));
            site.execution_targets.extend(targets);
            let manager = site
                .nodes
                .iter()
                .find(|node| node.site_manager.is_some())
                .expect("manager node")
                .node_id
                .clone();
            let fingerprint = CertificateFingerprint::from_bytes([7; 32]);
            let bootstrap = NodeFolderBootstrap::new(
                site.site_id.clone(),
                node_id,
                fingerprint.clone(),
                NodeFolderManager::new(manager, "https://10.0.0.1:7443", fingerprint)
                    .expect("manager"),
                node_observation_configuration(&site, node_id).expect("observation"),
                site.execution_targets
                    .iter()
                    .filter(|target| target.site_node_id == node_id)
                    .cloned()
                    .collect(),
                "10.0.0.7:7443".parse().expect("bind address"),
            )
            .expect("bootstrap");
            fs::write(
                root.join(WORKER_MANIFEST_FILE),
                dotcore_proto::yaml::to_string(&bootstrap).expect("serialize bootstrap"),
            )
            .expect("worker.yaml");
            drop(WorkerNodeCustody::try_acquire(&root).expect("create custody lock"));

            Self {
                _directory: directory,
                root,
                site_id: site.site_id,
            }
        }

        /// Native state directory of the execution target test workloads run on.
        fn native_target(&self) -> PathBuf {
            self.root.join("runtime/native").join(TEST_EXECUTION_TARGET)
        }

        fn rewrite_targets(&self, update: impl FnOnce(&mut Vec<ExecutionTarget>)) {
            let path = self.root.join(WORKER_MANIFEST_FILE);
            let mut bootstrap: NodeFolderBootstrap =
                dotcore_proto::yaml::from_str(&fs::read_to_string(&path).expect("read manifest"))
                    .expect("parse manifest");
            update(&mut bootstrap.realization_targets);
            fs::write(
                path,
                dotcore_proto::yaml::to_string(&bootstrap).expect("serialize manifest"),
            )
            .expect("rewrite manifest");
        }
    }

    /// Execution targets of a Mac node: a native model target and a Docker application target.
    fn native_targets(node_id: &str) -> Vec<ExecutionTarget> {
        build_execution_targets(
            node_id,
            &NodeTargetChoices {
                model_targets: vec![ModelTargetChoice::Apple {
                    device_id: dotcore_domain::APPLE_GPU_DEVICE_ID.to_owned(),
                    lifetime: NativeWorkloadLifetime::Persistent,
                }],
                application_targets: vec![ApplicationTargetChoice {
                    applications: InitialApplicationRuntime::Docker,
                    platform: ExecutionPlatform::linux_arm64(),
                    accelerator_device_ids: Vec::new(),
                }],
                accelerator_facts: Vec::new(),
                declared_machine: None,
            },
        )
        // Readable fixture identities, so tests name each target directly.
        .into_iter()
        .map(|mut target| {
            let role = match target.runtime_binding {
                Some(execution_target::RuntimeBinding::ModelRuntime(_)) => "model",
                _ => "application",
            };
            target.execution_target_id = format!("{node_id}-{role}-target");
            target
        })
        .collect()
    }

    #[derive(Default)]
    struct RecordingRuntime {
        calls: Arc<Mutex<Vec<(String, String, SiteRuntimeScope)>>>,
        fail_cleanup: bool,

        /// Directory the first cleanup creates, as a workload create starting meanwhile would.
        cleanup_creates: Mutex<Option<PathBuf>>,
    }

    impl SiteRuntimeCleanup for RecordingRuntime {
        fn inventory<'a>(
            &'a self,
            site_id: &'a str,
            scope: &'a SiteRuntimeScope,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeInventory>> + Send + 'a>>
        {
            self.calls.lock().expect("calls").push((
                "inventory".to_owned(),
                site_id.to_owned(),
                scope.clone(),
            ));
            Box::pin(std::future::ready(Ok(SiteRuntimeInventory {
                resources: vec![SiteRuntimeResource {
                    runtime: "Test Runtime",
                    description: "worker container".to_owned(),
                }],
                skipped: Vec::new(),
            })))
        }

        fn cleanup<'a>(
            &'a self,
            site_id: &'a str,
            scope: &'a SiteRuntimeScope,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<SiteRuntimeCleanupReport>> + Send + 'a>>
        {
            self.calls.lock().expect("calls").push((
                "cleanup".to_owned(),
                site_id.to_owned(),
                scope.clone(),
            ));
            if let Some(directory) = self.cleanup_creates.lock().expect("create").take() {
                fs::create_dir_all(directory).expect("create during cleanup");
            }
            let result = if self.fail_cleanup {
                Err(anyhow::anyhow!("injected cleanup failure"))
            } else {
                Ok(SiteRuntimeCleanupReport::default())
            };
            Box::pin(std::future::ready(result))
        }
    }
}
