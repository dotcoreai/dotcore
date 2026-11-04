//! Site store and node store: the durable current-state files of a site.
//!
//! The site store keeps owner records (accepted deployment status and lifecycle generation
//! high-water marks) in the site's `state.pb`. Every SiteNode keeps its own realization records,
//! cleanup evidence, and resolved launch identities in a separate node store, `node/state.pb`.
//! Both stores own the file envelope, its size bound and durability; the owners of the deployment
//! sections mutate and validate their records through the stores' typed transactions.

mod node;
mod site;
mod storage;

use std::collections::BTreeSet;

use dotcore_domain::{deployment::DeploymentOwner, valid_id};
use dotcore_proto::dotcore::{
    application::v1::CurrentApplicationDeployment,
    model::v1::CurrentModelDeployment,
    site::store::v1::{ManagerLifecycleRecord, SiteCurrentState},
};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::repository::SiteRepositoryError;
pub use node::{FileNodeState, NodeStoreLayoutError};
pub use node::{prepare_joined_node_store, prepare_site_node_store};
pub use site::{FileCurrentState, InMemoryCurrentState};

pub(crate) const FORMAT_VERSION: u32 = 2;
pub(crate) const MAX_STATE_BYTES: u64 = 4 * 1024 * 1024;

/// Failures of the site store and the node store: envelope validation, revision exhaustion, and
/// storage and durability outcomes. Section owners map them into their own error types.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("desired site failure")]
    SiteInput(#[source] SiteRepositoryError),

    #[error("current state is invalid")]
    InvalidState,

    #[error("current-state revision is exhausted")]
    RevisionExhausted,

    #[error("current state protobuf decode failed: {0}")]
    Decode(#[from] prost::DecodeError),

    #[error("current state is corrupt: {0}")]
    Corrupt(#[source] anyhow::Error),

    #[error("current-state format is unsupported")]
    UnsupportedFormat,

    #[error("current-state site ID does not match")]
    SiteMismatch,

    #[error("current state exceeds the storage limit")]
    LimitExceeded,

    #[error("Current state storage failed: {0}")]
    Storage(#[source] anyhow::Error),

    #[error("Current state storage outcome is uncertain: {0}")]
    OutcomeUncertain(#[source] anyhow::Error),

    #[error("current-state durability is uncertain")]
    Poisoned,

    #[error("current state is unavailable")]
    Unavailable,
}

impl StoreError {
    /// Returns the revision after `value`, failing once revisions are exhausted.
    pub(crate) fn next_revision(value: u64) -> Result<u64, Self> {
        value.checked_add(1).ok_or(Self::RevisionExhausted)
    }

    pub(crate) fn storage(
        operation: &'static str,
        path: impl std::fmt::Display,
        source: std::io::Error,
    ) -> Self {
        Self::Storage(
            anyhow::Error::new(source)
                .context(format!("current-state {operation} failed for {path}")),
        )
    }
}

/// Typed access to one store's snapshot for the owners of its deployment sections.
///
/// Owners read the committed snapshot and commit mutations of the records they own; each
/// mutation validates those records, and the store validates the envelope, its own sections and
/// the size before the change becomes visible.
pub trait CurrentStateStore: Send + Sync {
    type State: StoredCurrentState;

    /// Reads the committed snapshot.
    fn read<T>(&self, read: impl FnOnce(&Self::State) -> T) -> Result<T, StoreError>;

    /// Applies `operation` to a candidate copy and commits it once it is valid and durable.
    fn transact<T, E: From<StoreError>>(
        &self,
        operation: impl FnOnce(&mut Self::State) -> Result<T, E>,
    ) -> Result<T, E>;
}

/// Stored snapshot holding deployment current records that the controller repositories read and
/// write: the owner snapshot in the site's `state.pb` or a node snapshot in `node/state.pb`.
pub trait StoredCurrentState: Message + Clone + Default + 'static {
    fn site_id(&self) -> &str;

    fn storage_revision(&self) -> u64;

    fn storage_revision_mut(&mut self) -> &mut u64;

    fn model_deployments(&self) -> &[CurrentModelDeployment];

    fn model_deployments_mut(&mut self) -> &mut Vec<CurrentModelDeployment>;

    fn application_deployments(&self) -> &[CurrentApplicationDeployment];

    fn application_deployments_mut(&mut self) -> &mut Vec<CurrentApplicationDeployment>;

    /// Validates the format version, identities, and every section other than deployment records.
    fn validate_stored(&self) -> Result<(), StoreError>;
}

impl StoredCurrentState for SiteCurrentState {
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
        validate_envelope(self, &self.site_id)
    }
}

pub(crate) fn validate_encoded_size<M: Message>(message: &M) -> Result<(), StoreError> {
    if message.encoded_len() as u64 > MAX_STATE_BYTES {
        return Err(StoreError::LimitExceeded);
    }
    Ok(())
}

pub(crate) fn new_envelope(site_id: String) -> Result<SiteCurrentState, StoreError> {
    validate_site_id(&site_id)?;
    Ok(SiteCurrentState {
        format_version: FORMAT_VERSION,
        site_id,
        storage_revision: 0,
        model_deployments: Vec::new(),
        application_deployments: Vec::new(),
        manager_model_lifecycle_records: Vec::new(),
        manager_application_lifecycle_records: Vec::new(),
    })
}

pub(crate) fn validate_envelope(
    state: &SiteCurrentState,
    expected_site_id: &str,
) -> Result<(), StoreError> {
    if state.format_version != FORMAT_VERSION {
        return Err(StoreError::UnsupportedFormat);
    }
    validate_site_id(&state.site_id)?;
    if state.site_id != expected_site_id {
        return Err(StoreError::SiteMismatch);
    }
    validate_lifecycle_records(state)?;
    Ok(())
}

/// Advances one deployment's durable lifecycle high-water mark in `state` and returns its record.
/// The returned high-water is the new never-reused generation; the first reservation is 1.
pub fn reserve_lifecycle_generation(
    state: &mut SiteCurrentState,
    owner: DeploymentOwner,
    deployment_id: &str,
) -> Result<ManagerLifecycleRecord, StoreError> {
    if !valid_id(deployment_id) {
        return Err(StoreError::InvalidState);
    }
    let records = lifecycle_records_mut(state, owner);
    let record = match records
        .iter_mut()
        .find(|record| record.deployment_id == deployment_id)
    {
        Some(record) => {
            record.generation_high_water = StoreError::next_revision(record.generation_high_water)?;
            record.clone()
        }
        None => {
            let record = ManagerLifecycleRecord {
                deployment_id: deployment_id.to_owned(),
                generation_high_water: 1,
            };
            records.push(record.clone());
            record
        }
    };
    records.sort_by(|left, right| left.deployment_id.cmp(&right.deployment_id));
    state.storage_revision = StoreError::next_revision(state.storage_revision)?;
    Ok(record)
}

/// Returns one owner's manager lifecycle generation records.
pub(crate) fn lifecycle_records(
    state: &SiteCurrentState,
    owner: DeploymentOwner,
) -> &[ManagerLifecycleRecord] {
    match owner {
        DeploymentOwner::Model => &state.manager_model_lifecycle_records,
        DeploymentOwner::Application => &state.manager_application_lifecycle_records,
    }
}

/// Returns one owner's mutable manager lifecycle generation records.
pub(crate) fn lifecycle_records_mut(
    state: &mut SiteCurrentState,
    owner: DeploymentOwner,
) -> &mut Vec<ManagerLifecycleRecord> {
    match owner {
        DeploymentOwner::Model => &mut state.manager_model_lifecycle_records,
        DeploymentOwner::Application => &mut state.manager_application_lifecycle_records,
    }
}

fn validate_lifecycle_records(state: &SiteCurrentState) -> Result<(), StoreError> {
    for owner in [DeploymentOwner::Model, DeploymentOwner::Application] {
        let mut identities = BTreeSet::new();
        for record in lifecycle_records(state, owner) {
            if !valid_id(&record.deployment_id)
                || record.generation_high_water == 0
                || !identities.insert(record.deployment_id.as_str())
            {
                return Err(StoreError::InvalidState);
            }
        }
    }
    Ok(())
}

/// Derives the stable random-looking deployment identity namespace for one site.
/// The namespace is independent of mutable site declarations and survives ordinary rewrites.
pub fn model_deployment_identity_namespace(site_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"dotcore.model-deployment-namespace.v1\0");
    hasher.update(site_id.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Digests one cleanup evidence identity: exactly its deployment and teardown lifecycle generation.
pub fn cleanup_evidence_digest(model_deployment_id: &str, lifecycle_generation: u64) -> String {
    evidence_identity_digest(
        b"dotcore.worker-model-cleanup-evidence.v2\0",
        model_deployment_id,
        lifecycle_generation,
    )
}

/// Digests one application cleanup evidence identity: exactly its deployment and teardown
/// lifecycle generation, in a domain distinct from model evidence.
pub(crate) fn application_cleanup_evidence_digest(
    application_deployment_id: &str,
    lifecycle_generation: u64,
) -> String {
    evidence_identity_digest(
        b"dotcore.worker-application-cleanup-evidence.v1\0",
        application_deployment_id,
        lifecycle_generation,
    )
}

fn evidence_identity_digest(
    domain: &[u8],
    deployment_id: &str,
    lifecycle_generation: u64,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update((deployment_id.len() as u64).to_be_bytes());
    hasher.update(deployment_id.as_bytes());
    hasher.update(lifecycle_generation.to_be_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_site_id(site_id: &str) -> Result<(), StoreError> {
    if valid_id(site_id) {
        Ok(())
    } else {
        Err(StoreError::InvalidState)
    }
}
