//! Node store persistence: the `node/state.pb` custody record of one SiteNode.
//!
//! A node keeps the realization records, cleanup evidence, and resolved launch identities of the
//! deployments it hosts here, apart from the site manager's owner records, so sharing a process
//! or directory with the manager never mixes node state into owner state. A site or node folder
//! that still keeps node state in its root `state.pb` is rejected with a reset instruction.

use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

use dotcore_domain::{SiteNodeId, valid_id};
use dotcore_proto::dotcore::{
    application::v1::CurrentApplicationDeployment,
    model::v1::CurrentModelDeployment,
    site::store::v1::{
        NodeApplicationCleanupEvidence, NodeApplicationRealization, NodeCurrentState,
        NodeModelCleanupEvidence, NodeResolvedLaunchIdentities,
    },
};

use crate::{
    atomic::StdFileSystem,
    store::{
        CurrentStateStore, StoreError, StoredCurrentState,
        storage::{StateFile, acquire_lock, read_stored},
        validate_site_id,
    },
};

/// Directory of the node store beneath a site or node folder root.
const NODE_STORE_DIRECTORY: &str = "node";

const STATE_FILE: &str = "state.pb";

const NODE_FORMAT_VERSION: u32 = 1;

/// File-backed node custody state of one SiteNode, written to `state.pb` in its node store.
#[derive(Clone)]
pub struct FileNodeState {
    file: StateFile<NodeCurrentState>,

    /// Counts committed changes of the state the node reports: its model and application
    /// records, realization records, cleanup evidence, and resolved launch identities.
    changed: Arc<tokio::sync::watch::Sender<u64>>,
}

impl FileNodeState {
    /// Opens the node store in `directory`, creating it when absent, under exclusive custody.
    /// Retained state must belong to exactly `site_id` and `site_node_id`.
    pub fn open(
        directory: impl AsRef<Path>,
        site_id: impl Into<String>,
        site_node_id: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let directory = directory.as_ref();
        let (site_id, site_node_id) = (site_id.into(), site_node_id.into());
        fs::create_dir_all(directory).map_err(|source| {
            StoreError::storage("node store creation", directory.display(), source)
        })?;
        let path = directory.join(STATE_FILE);
        let lock = acquire_lock(&path)?;
        let state = match read_stored::<NodeCurrentState>(&path)? {
            Some(state) => {
                state.validate_stored()?;
                if state.site_id != site_id {
                    return Err(StoreError::SiteMismatch);
                }
                if state.site_node_id != site_node_id {
                    return Err(StoreError::Corrupt(anyhow::anyhow!(
                        "node state belongs to node {:?}, not {site_node_id:?}",
                        state.site_node_id
                    )));
                }
                state
            }
            None => {
                let state = NodeCurrentState {
                    format_version: NODE_FORMAT_VERSION,
                    site_id,
                    site_node_id,
                    ..Default::default()
                };
                state.validate_stored()?;
                state
            }
        };
        Ok(Self {
            file: StateFile::new(state, path, Arc::new(StdFileSystem), lock),
            changed: Arc::new(tokio::sync::watch::channel(0).0),
        })
    }

    /// Returns a receiver whose value changes after each committed change of the node's reported
    /// state, so its status is reported and its pinned artifacts kept without waiting for the next
    /// view publication.
    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changed.subscribe()
    }

    /// Advances the change signal after a successful commit.
    fn committed<T, E>(&self, result: Result<T, E>) -> Result<T, E> {
        if result.is_ok() {
            self.changed
                .send_modify(|count| *count = count.wrapping_add(1));
        }
        result
    }

    /// Removes the deployment's local current record and records its cleanup evidence in one
    /// state transaction, so a crash leaves either the record or the complete evidence. Evidence
    /// is keyed by deployment and lifecycle generation: a repeated confirmation returns the
    /// retained evidence unchanged, because a later pass over an already-removed workload can no
    /// longer observe the realization generation the first pass recorded.
    pub fn confirm_node_model_cleanup(
        &self,
        model_deployment_id: &str,
        lifecycle_generation: u64,
        realization_generation: Option<u64>,
    ) -> Result<NodeModelCleanupEvidence, StoreError> {
        let is_key = |evidence: &NodeModelCleanupEvidence| {
            evidence.model_deployment_id == model_deployment_id
                && evidence.lifecycle_generation == lifecycle_generation
        };
        // Settled cleanup is returned without a transaction so repeated passes never rewrite the
        // state file.
        let settled = self.file.read(|state| {
            let has_record = state
                .model_deployments
                .iter()
                .any(|record| record.model_deployment_id == model_deployment_id);
            state
                .model_cleanup_evidence
                .iter()
                .find(|evidence| is_key(evidence))
                .filter(|_| !has_record)
                .cloned()
        })?;
        if let Some(existing) = settled {
            return Ok(existing);
        }
        if !valid_id(model_deployment_id) || lifecycle_generation == 0 {
            return Err(StoreError::InvalidState);
        }
        let committed = self.file.transact(|state| {
            state
                .model_deployments
                .retain(|record| record.model_deployment_id != model_deployment_id);
            state
                .model_resolved_launch_identities
                .retain(|record| record.deployment_id != model_deployment_id);
            let evidence = match state
                .model_cleanup_evidence
                .iter()
                .find(|evidence| is_key(evidence))
            {
                Some(existing) => existing.clone(),
                None => {
                    let evidence = NodeModelCleanupEvidence {
                        model_deployment_id: model_deployment_id.to_owned(),
                        lifecycle_generation,
                        realization_generation,
                        never_realized: realization_generation.is_none(),
                        acknowledged: false,
                    };
                    state.model_cleanup_evidence.push(evidence.clone());
                    sort_retained_evidence(&mut state.model_cleanup_evidence);
                    evidence
                }
            };
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(evidence)
        });
        self.committed(committed)
    }

    /// Returns durable cleanup evidence retained for status publication.
    pub fn node_model_cleanup_evidence(&self) -> Result<Vec<NodeModelCleanupEvidence>, StoreError> {
        self.retained_cleanup_evidence()
    }

    /// Marks retained evidence acknowledged by the manager so it is never reported again. The
    /// evidence stays retained while its teardown remains installed, so reconciliation of that
    /// unchanged teardown cannot recreate it.
    pub fn acknowledge_node_cleanup_evidence(
        &self,
        acknowledged: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        self.acknowledge_retained_evidence::<NodeModelCleanupEvidence>(acknowledged)
    }

    /// Deletes acknowledged evidence whose teardown is no longer installed. `installed` names
    /// every deployment and lifecycle generation of the installed view's teardowns.
    pub fn retire_acknowledged_cleanup_evidence(
        &self,
        installed: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        self.retire_retained_evidence::<NodeModelCleanupEvidence>(installed)
    }

    /// Returns every model deployment's launch identities this node resolved once.
    pub fn node_model_resolved_launch_identities(
        &self,
    ) -> Result<Vec<NodeResolvedLaunchIdentities>, StoreError> {
        self.file
            .read(|state| state.model_resolved_launch_identities.clone())
    }

    /// Retains the launch identities this node resolved for one model deployment, before any
    /// workload launches from them. They stay until the deployment's cleanup is confirmed.
    pub fn retain_node_model_resolved_launch_identities(
        &self,
        record: NodeResolvedLaunchIdentities,
    ) -> Result<(), StoreError> {
        let committed = self.file.transact(|state| {
            let records = &mut state.model_resolved_launch_identities;
            records.retain(|retained| retained.deployment_id != record.deployment_id);
            records.push(record);
            records.sort_by(|left, right| left.deployment_id.cmp(&right.deployment_id));
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(())
        });
        self.committed(committed)
    }

    /// Returns the realization record of one application deployment, if this node began
    /// realizing it.
    pub fn node_application_realization(
        &self,
        application_deployment_id: &str,
    ) -> Result<Option<NodeApplicationRealization>, StoreError> {
        self.file.read(|state| {
            state
                .application_realizations
                .iter()
                .find(|record| record.application_deployment_id == application_deployment_id)
                .cloned()
        })
    }

    /// Returns every application realization record.
    pub fn node_application_realizations(
        &self,
    ) -> Result<Vec<NodeApplicationRealization>, StoreError> {
        self.file
            .read(|state| state.application_realizations.clone())
    }

    /// Creates or replaces one application realization record; an unchanged record is not
    /// rewritten.
    pub fn record_node_application_realization(
        &self,
        realization: NodeApplicationRealization,
    ) -> Result<(), StoreError> {
        if self
            .node_application_realization(&realization.application_deployment_id)?
            .as_ref()
            == Some(&realization)
        {
            return Ok(());
        }
        self.committed(self.file.transact(|state| {
            let records = &mut state.application_realizations;
            records.retain(|record| {
                record.application_deployment_id != realization.application_deployment_id
            });
            records.push(realization);
            records.sort_by(|left, right| {
                left.application_deployment_id
                    .cmp(&right.application_deployment_id)
            });
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(())
        }))
    }

    /// Removes the application deployment's local current and realization records and records
    /// its cleanup evidence in one state transaction, so a crash leaves either the records or the
    /// complete evidence. A repeated confirmation returns the retained evidence unchanged.
    pub fn confirm_node_application_cleanup(
        &self,
        application_deployment_id: &str,
        lifecycle_generation: u64,
    ) -> Result<NodeApplicationCleanupEvidence, StoreError> {
        let is_key = |evidence: &NodeApplicationCleanupEvidence| {
            evidence.application_deployment_id == application_deployment_id
                && evidence.lifecycle_generation == lifecycle_generation
        };
        let settled =
            self.file.read(|state| {
                let has_record =
                    state.application_deployments.iter().any(|record| {
                        record.application_deployment_id == application_deployment_id
                    }) || state.application_realizations.iter().any(|record| {
                        record.application_deployment_id == application_deployment_id
                    });
                state
                    .application_cleanup_evidence
                    .iter()
                    .find(|evidence| is_key(evidence))
                    .filter(|_| !has_record)
                    .cloned()
            })?;
        if let Some(existing) = settled {
            return Ok(existing);
        }
        if !valid_id(application_deployment_id) || lifecycle_generation == 0 {
            return Err(StoreError::InvalidState);
        }
        self.committed(self.file.transact(|state| {
            state
                .application_deployments
                .retain(|record| record.application_deployment_id != application_deployment_id);
            let realization_generation = state
                .application_realizations
                .iter()
                .find(|record| record.application_deployment_id == application_deployment_id)
                .and_then(|record| record.realization_generation);
            state
                .application_realizations
                .retain(|record| record.application_deployment_id != application_deployment_id);
            let evidence = match state
                .application_cleanup_evidence
                .iter()
                .find(|evidence| is_key(evidence))
            {
                Some(existing) => existing.clone(),
                None => {
                    let evidence = NodeApplicationCleanupEvidence {
                        application_deployment_id: application_deployment_id.to_owned(),
                        lifecycle_generation,
                        realization_generation,
                        never_realized: realization_generation.is_none(),
                        acknowledged: false,
                    };
                    state.application_cleanup_evidence.push(evidence.clone());
                    sort_retained_evidence(&mut state.application_cleanup_evidence);
                    evidence
                }
            };
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(evidence)
        }))
    }

    /// Returns durable application cleanup evidence retained for status publication.
    pub fn node_application_cleanup_evidence(
        &self,
    ) -> Result<Vec<NodeApplicationCleanupEvidence>, StoreError> {
        self.retained_cleanup_evidence()
    }

    /// Marks application evidence acknowledged by the manager so it is never reported again.
    pub fn acknowledge_node_application_cleanup_evidence(
        &self,
        acknowledged: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        self.acknowledge_retained_evidence::<NodeApplicationCleanupEvidence>(acknowledged)
    }

    /// Deletes acknowledged application evidence whose teardown is no longer installed.
    pub fn retire_acknowledged_application_cleanup_evidence(
        &self,
        installed: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        self.retire_retained_evidence::<NodeApplicationCleanupEvidence>(installed)
    }

    fn retained_cleanup_evidence<E: RetainedCleanupEvidence>(&self) -> Result<Vec<E>, StoreError> {
        self.file.read(|state| E::retained(state).to_vec())
    }

    fn acknowledge_retained_evidence<E: RetainedCleanupEvidence>(
        &self,
        acknowledged: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        let pending =
            |evidence: &E| !evidence.acknowledged() && acknowledged.contains(&evidence.key());
        if !self.retained_cleanup_evidence::<E>()?.iter().any(pending) {
            return Ok(());
        }
        self.file.transact(|state| {
            for evidence in E::retained_mut(state) {
                if pending(evidence) {
                    evidence.acknowledge();
                }
            }
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(())
        })
    }

    fn retire_retained_evidence<E: RetainedCleanupEvidence>(
        &self,
        installed: &BTreeSet<(String, u64)>,
    ) -> Result<(), StoreError> {
        let retired =
            |evidence: &E| evidence.acknowledged() && !installed.contains(&evidence.key());
        if !self.retained_cleanup_evidence::<E>()?.iter().any(retired) {
            return Ok(());
        }
        self.file.transact(|state| {
            E::retained_mut(state).retain(|evidence| !retired(evidence));
            state.storage_revision = StoreError::next_revision(state.storage_revision)?;
            Ok(())
        })
    }
}

impl CurrentStateStore for FileNodeState {
    type State = NodeCurrentState;

    fn read<T>(&self, read: impl FnOnce(&NodeCurrentState) -> T) -> Result<T, StoreError> {
        self.file.read(read)
    }

    /// Commits the mutation once durable and raises the change signal.
    fn transact<T, E: From<StoreError>>(
        &self,
        operation: impl FnOnce(&mut NodeCurrentState) -> Result<T, E>,
    ) -> Result<T, E> {
        self.committed(self.file.transact(operation))
    }
}

/// Checks that a full site root keeps node state in its node store and creates that store's
/// directory, returning it. An owner `state.pb` without a node store is a layout this backend
/// does not read; it is rejected before anything at the root is written.
pub fn prepare_site_node_store(site_root: &Path) -> Result<PathBuf, NodeStoreLayoutError> {
    let directory = site_root.join(NODE_STORE_DIRECTORY);
    if layout_entry_exists(&site_root.join(STATE_FILE))? && !layout_entry_exists(&directory)? {
        return Err(NodeStoreLayoutError::UnsupportedSite {
            site_root: site_root.to_owned(),
        });
    }
    create_node_store_directory(directory)
}

/// Checks that a node folder root keeps no node state outside its node store and creates that
/// store's directory, returning it. A root `state.pb` is a layout this backend does not read.
pub fn prepare_joined_node_store(node_root: &Path) -> Result<PathBuf, NodeStoreLayoutError> {
    if layout_entry_exists(&node_root.join(STATE_FILE))? {
        return Err(NodeStoreLayoutError::UnsupportedNodeFolder {
            node_root: node_root.to_owned(),
        });
    }
    create_node_store_directory(node_root.join(NODE_STORE_DIRECTORY))
}

/// A site or node folder root whose state layout this backend cannot start from.
#[derive(Debug, thiserror::Error)]
pub enum NodeStoreLayoutError {
    #[error(
        "site at {site_root} has an owner state.pb without a node store (node/), a state layout \
         this backend does not read; stop the backend and reset the site"
    )]
    UnsupportedSite { site_root: PathBuf },

    #[error(
        "node folder at {node_root} keeps node state in state.pb instead of its node store \
         (node/), a state layout this backend does not read; remove the node folder and \
         join the site again"
    )]
    UnsupportedNodeFolder { node_root: PathBuf },

    #[error("node store layout inspection failed at {path}")]
    Storage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl StoredCurrentState for NodeCurrentState {
    fn site_id(&self) -> &str {
        &self.site_id
    }

    fn storage_revision(&self) -> u64 {
        self.storage_revision
    }

    fn storage_revision_mut(&mut self) -> &mut u64 {
        &mut self.storage_revision
    }

    fn model_deployments(&self) -> &[CurrentModelDeployment] {
        &self.model_deployments
    }

    fn model_deployments_mut(&mut self) -> &mut Vec<CurrentModelDeployment> {
        &mut self.model_deployments
    }

    fn application_deployments(&self) -> &[CurrentApplicationDeployment] {
        &self.application_deployments
    }

    fn application_deployments_mut(&mut self) -> &mut Vec<CurrentApplicationDeployment> {
        &mut self.application_deployments
    }

    fn validate_stored(&self) -> Result<(), StoreError> {
        if self.format_version != NODE_FORMAT_VERSION {
            return Err(StoreError::UnsupportedFormat);
        }
        validate_site_id(&self.site_id)?;
        if SiteNodeId::parse(&self.site_node_id).is_err() {
            return Err(StoreError::InvalidState);
        }
        validate_custody_sections(self)
    }
}

/// Node-retained cleanup evidence of one deployment owner, keyed by deployment and teardown
/// lifecycle generation.
trait RetainedCleanupEvidence: Clone + Sized {
    fn retained(state: &NodeCurrentState) -> &[Self];

    fn retained_mut(state: &mut NodeCurrentState) -> &mut Vec<Self>;

    fn key(&self) -> (String, u64);

    fn acknowledged(&self) -> bool;

    fn acknowledge(&mut self);
}

impl RetainedCleanupEvidence for NodeModelCleanupEvidence {
    fn retained(state: &NodeCurrentState) -> &[Self] {
        &state.model_cleanup_evidence
    }

    fn retained_mut(state: &mut NodeCurrentState) -> &mut Vec<Self> {
        &mut state.model_cleanup_evidence
    }

    fn key(&self) -> (String, u64) {
        (self.model_deployment_id.clone(), self.lifecycle_generation)
    }

    fn acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn acknowledge(&mut self) {
        self.acknowledged = true;
    }
}

impl RetainedCleanupEvidence for NodeApplicationCleanupEvidence {
    fn retained(state: &NodeCurrentState) -> &[Self] {
        &state.application_cleanup_evidence
    }

    fn retained_mut(state: &mut NodeCurrentState) -> &mut Vec<Self> {
        &mut state.application_cleanup_evidence
    }

    fn key(&self) -> (String, u64) {
        (
            self.application_deployment_id.clone(),
            self.lifecycle_generation,
        )
    }

    fn acknowledged(&self) -> bool {
        self.acknowledged
    }

    fn acknowledge(&mut self) {
        self.acknowledged = true;
    }
}

fn sort_retained_evidence<E: RetainedCleanupEvidence>(evidence: &mut [E]) {
    evidence.sort_by_key(RetainedCleanupEvidence::key);
}

fn validate_custody_sections(state: &NodeCurrentState) -> Result<(), StoreError> {
    let mut evidence_keys = BTreeSet::new();
    for evidence in &state.model_cleanup_evidence {
        let key = (
            evidence.model_deployment_id.as_str(),
            evidence.lifecycle_generation,
        );
        if !valid_id(&evidence.model_deployment_id)
            || evidence.lifecycle_generation == 0
            || evidence.never_realized != evidence.realization_generation.is_none()
            || !evidence_keys.insert(key)
        {
            return Err(StoreError::InvalidState);
        }
    }
    let mut application_evidence_keys = BTreeSet::new();
    for evidence in &state.application_cleanup_evidence {
        let key = (
            evidence.application_deployment_id.as_str(),
            evidence.lifecycle_generation,
        );
        if !valid_id(&evidence.application_deployment_id)
            || evidence.lifecycle_generation == 0
            || evidence.never_realized != evidence.realization_generation.is_none()
            || !application_evidence_keys.insert(key)
        {
            return Err(StoreError::InvalidState);
        }
    }
    let mut realizations = BTreeSet::new();
    for realization in &state.application_realizations {
        if !valid_id(&realization.application_deployment_id)
            || !realizations.insert(realization.application_deployment_id.as_str())
            || !valid_retained_pins(&realization.resolved_launch_identities)
        {
            return Err(StoreError::InvalidState);
        }
    }
    let mut pinned = BTreeSet::new();
    for record in &state.model_resolved_launch_identities {
        if !valid_id(&record.deployment_id)
            || !pinned.insert(record.deployment_id.as_str())
            || record.resolved_launch_identities.is_empty()
            || !valid_retained_pins(&record.resolved_launch_identities)
        {
            return Err(StoreError::InvalidState);
        }
    }
    Ok(())
}

/// Whether retained pins each name a distinct valid selection with a local image ID or a store
/// path; the node store does not keep which kind each selection needs.
fn valid_retained_pins(
    pins: &[dotcore_proto::dotcore::deployment::v1::NodeResolvedLaunchIdentity],
) -> bool {
    use dotcore_domain::deployment::launch_pin::PinKind;

    let mut keys = BTreeSet::new();
    pins.iter().all(|pin| {
        valid_id(&pin.selection_key)
            && keys.insert(pin.selection_key.as_str())
            && [PinKind::LocalImage, PinKind::NixStorePath]
                .into_iter()
                .any(|kind| kind.admits(&pin.launch_identity))
    })
}

fn layout_entry_exists(path: &Path) -> Result<bool, NodeStoreLayoutError> {
    path.try_exists()
        .map_err(|source| NodeStoreLayoutError::Storage {
            path: path.to_owned(),
            source,
        })
}

fn create_node_store_directory(directory: PathBuf) -> Result<PathBuf, NodeStoreLayoutError> {
    fs::create_dir_all(&directory).map_err(|source| NodeStoreLayoutError::Storage {
        path: directory.clone(),
        source,
    })?;
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs};

    use dotcore_proto::dotcore::{
        model::v1::{CurrentModelDeployment, ModelDeploymentState, ModelDeploymentStatus},
        site::store::v1::{NodeCurrentState, NodeModelCleanupEvidence},
    };
    use prost::Message;

    use super::{
        FileNodeState, NodeStoreLayoutError, prepare_joined_node_store, prepare_site_node_store,
    };
    use crate::{
        store::StoreError,
        test_support::{stored, write_model_record},
    };

    #[test]
    fn node_state_persists_in_its_node_store_and_rejects_another_identity() {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("Site root");
        let store = directory.path().join("node");
        let state = FileNodeState::open(&store, "site-one", "node-one").expect("node state");
        write_model_record(&state, deployment("model-one")).expect("realization record");
        assert_lock_contended(FileNodeState::open(&store, "site-one", "node-one"));
        drop(state);

        let persisted = NodeCurrentState::decode(
            fs::read(store.join("state.pb"))
                .expect("node state bytes")
                .as_slice(),
        )
        .expect("node state protobuf");
        assert_eq!(
            (
                persisted.site_id.as_str(),
                persisted.site_node_id.as_str(),
                persisted.storage_revision
            ),
            ("site-one", "node-one", 1)
        );
        assert!(!directory.path().join("state.pb").exists());
        let reopened = FileNodeState::open(&store, "site-one", "node-one").expect("restart");
        assert_eq!(
            stored(&reopened).expect("snapshot").model_deployments,
            [deployment("model-one")]
        );
        drop(reopened);

        assert!(matches!(
            FileNodeState::open(&store, "site-two", "node-one"),
            Err(StoreError::SiteMismatch)
        ));
        assert!(matches!(
            FileNodeState::open(&store, "site-one", "node-two"),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn acknowledged_cleanup_evidence_is_retained_until_its_teardown_leaves_the_view() {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("Site root");
        let store = directory.path().join("node");
        let current = FileNodeState::open(&store, "site-one", "node-one").expect("node state");
        write_model_record(&current, deployment("model-one")).expect("ordinary current state");
        let before = stored(&current).expect("state before cleanup");

        let evidence = current
            .confirm_node_model_cleanup("model-one", 3, Some(9))
            .expect("cleanup evidence");
        let confirmed = stored(&current).expect("state after cleanup");
        let repeated = current
            .confirm_node_model_cleanup("model-one", 3, Some(1))
            .expect("repeated cleanup with another realization generation");

        assert_eq!(
            evidence,
            NodeModelCleanupEvidence {
                model_deployment_id: "model-one".to_owned(),
                lifecycle_generation: 3,
                realization_generation: Some(9),
                never_realized: false,
                acknowledged: false,
            }
        );
        assert_eq!(repeated, evidence);
        assert!(confirmed.model_deployments.is_empty());
        assert_eq!(confirmed.storage_revision, before.storage_revision + 1);
        assert_eq!(
            stored(&current).expect("unchanged state").storage_revision,
            confirmed.storage_revision
        );

        let acknowledged = BTreeSet::from([("model-one".to_owned(), 3)]);
        current
            .acknowledge_node_cleanup_evidence(&acknowledged)
            .expect("acknowledged evidence");
        drop(current);
        let reopened = FileNodeState::open(&store, "site-one", "node-one").expect("restart");
        let marked = NodeModelCleanupEvidence {
            acknowledged: true,
            ..evidence
        };
        assert_eq!(
            reopened
                .node_model_cleanup_evidence()
                .expect("retained evidence"),
            std::slice::from_ref(&marked)
        );
        assert_eq!(
            reopened
                .confirm_node_model_cleanup("model-one", 3, None)
                .expect("reconciling the installed teardown again"),
            marked
        );

        reopened
            .retire_acknowledged_cleanup_evidence(&acknowledged)
            .expect("teardown still installed");
        assert_eq!(
            reopened.node_model_cleanup_evidence().expect("evidence"),
            [marked]
        );
        reopened
            .retire_acknowledged_cleanup_evidence(&BTreeSet::new())
            .expect("teardown withdrawn from the view");
        drop(reopened);
        let retired = FileNodeState::open(&store, "site-one", "node-one").expect("final restart");
        assert!(
            retired
                .node_model_cleanup_evidence()
                .expect("retired evidence")
                .is_empty()
        );
    }

    #[test]
    fn retained_engine_pins_round_trip_as_image_ids_or_store_paths_only() {
        use dotcore_proto::dotcore::{
            deployment::v1::NodeResolvedLaunchIdentity,
            site::store::v1::NodeResolvedLaunchIdentities,
        };

        let directory = dotcore_domain::test_support::canonical_tempdir().expect("Site root");
        let store = directory.path().join("node");
        let record = |deployment_id: &str, launch_identity: &str| NodeResolvedLaunchIdentities {
            deployment_id: deployment_id.to_owned(),
            resolved_launch_identities: vec![NodeResolvedLaunchIdentity {
                selection_key: "engine".to_owned(),
                launch_identity: launch_identity.to_owned(),
            }],
        };
        let pins = [
            record("model-image", &format!("sha256:{}", "1".repeat(64))),
            record("model-nix", dotcore_domain::test_support::nix::STORE_PATH),
        ];
        let state = FileNodeState::open(&store, "site-one", "node-one").expect("node state");
        for pin in &pins {
            state
                .retain_node_model_resolved_launch_identities(pin.clone())
                .expect("retained pin");
        }
        drop(state);

        let reopened = FileNodeState::open(&store, "site-one", "node-one").expect("restart");
        assert_eq!(
            reopened
                .node_model_resolved_launch_identities()
                .expect("retained pins"),
            pins
        );
        drop(reopened);

        for invalid in ["registry.test/engine:dev", "/tmp/llama-server"] {
            let mut persisted = NodeCurrentState::decode(
                fs::read(store.join("state.pb"))
                    .expect("node state bytes")
                    .as_slice(),
            )
            .expect("node state protobuf");
            persisted.model_resolved_launch_identities = vec![record("model-one", invalid)];
            fs::write(store.join("state.pb"), persisted.encode_to_vec()).expect("edited state");
            assert!(
                FileNodeState::open(&store, "site-one", "node-one").is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn site_layout_without_a_node_store_is_rejected_before_any_write() {
        let directory = dotcore_domain::test_support::canonical_tempdir().expect("Site root");
        let root = directory.path();

        assert_eq!(
            prepare_site_node_store(root).expect("fresh site"),
            root.join("node")
        );
        fs::write(root.join("state.pb"), b"owner state").expect("owner state");
        assert_eq!(
            prepare_site_node_store(root).expect("Site with a node store"),
            root.join("node")
        );

        fs::remove_dir(root.join("node")).expect("pre-split layout");
        let error = prepare_site_node_store(root).expect_err("pre-split site");
        assert!(matches!(
            error,
            NodeStoreLayoutError::UnsupportedSite { .. }
        ));
        assert!(
            error
                .to_string()
                .ends_with("; stop the backend and reset the site")
        );
        assert!(!root.join("node").exists());
        assert_eq!(
            fs::read(root.join("state.pb")).expect("unchanged state"),
            b"owner state"
        );
    }

    #[test]
    fn node_folder_layout_with_root_state_is_rejected_before_any_write() {
        let directory =
            dotcore_domain::test_support::canonical_tempdir().expect("node folder root");
        let root = directory.path();

        assert_eq!(
            prepare_joined_node_store(root).expect("fresh node folder"),
            root.join("node")
        );
        fs::write(root.join("state.pb"), b"worker state").expect("pre-split worker state");
        let error = prepare_joined_node_store(root).expect_err("pre-split node folder");

        assert!(matches!(
            error,
            NodeStoreLayoutError::UnsupportedNodeFolder { .. }
        ));
        assert!(
            error
                .to_string()
                .ends_with("; remove the node folder and join the site again")
        );
    }

    fn assert_lock_contended(result: Result<FileNodeState, StoreError>) {
        let Err(StoreError::Storage(source)) = result else {
            panic!("a second node-state owner must be rejected");
        };
        assert_eq!(source.to_string(), "current state is already locked");
    }

    fn deployment(id: &str) -> CurrentModelDeployment {
        CurrentModelDeployment {
            model_deployment_id: id.to_owned(),
            reconciled_lifecycle_generation: None,
            record_revision: 0,
            recovery_barrier: None,
            state: Some(ModelDeploymentState {
                status: ModelDeploymentStatus::Loading as i32,
                status_message: "Loading".to_owned(),
                ..Default::default()
            }),
        }
    }
}
