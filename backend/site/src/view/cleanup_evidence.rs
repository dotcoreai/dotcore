//! Classifies worker cleanup evidence against the manager's accepted site, for every deployment
//! owner alike, so model and application teardowns finalize under one rule.
//!
//! Evidence for deployment X at lifecycle generation g from SiteNode N removes X only when X is
//! in teardown at exactly g and placed on N, and is acknowledged only once that removal is
//! durable; for example evidence (`open-webui-1`, 4) from `worker-one` finalizes a generation-4
//! teardown placed there. Wrong-node, future-generation, and non-teardown evidence is rejected
//! with no effect, while obsolete evidence and evidence for an absent deployment need none.

use std::collections::BTreeSet;

use dotcore_domain::deployment::DeploymentOwner;
use tracing::{debug, warn};

use crate::{
    repository::{DeploymentTeardownIdentity, DeploymentTeardownTransaction},
    store::{FileCurrentState, StoreError},
};

/// Manager decision for one cleanup evidence entry before any durability outcome is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupDecision {
    /// Acknowledged only once accepted site memory is durable: the matching teardown removal
    /// was staged, or the deployment is already absent.
    AfterDurability,

    /// Obsolete evidence for a generation the deployment already moved past; nothing to do.
    WithoutEffect,

    /// Wrong node, future generation, or a generation that is not a teardown.
    Rejected,
}

impl CleanupDecision {
    /// Whether the evidence is acknowledged, given whether staged removals became durable.
    pub fn acknowledged(self, durable: bool) -> bool {
        match self {
            Self::AfterDurability => durable,
            Self::WithoutEffect => true,
            Self::Rejected => false,
        }
    }
}

/// Classifies evidence (`deployment_id`, `generation`) of one owner from `node_id` against
/// accepted site memory, staging the removal when the deployment is in teardown at exactly that
/// generation and placed on that node.
pub fn classify_cleanup_evidence(
    teardowns: &mut DeploymentTeardownTransaction<'_>,
    state: &FileCurrentState,
    owner: DeploymentOwner,
    node_id: &str,
    deployment_id: &str,
    generation: u64,
) -> Result<CleanupDecision, StoreError> {
    let Some((declared_generation, placement_node)) =
        declared_deployment(teardowns, owner, deployment_id)
    else {
        return Ok(CleanupDecision::AfterDurability);
    };
    let reason = if placement_node.as_deref() != Some(node_id) {
        "placed_on_another_node"
    } else if generation == declared_generation {
        if teardowns.remove_teardown(&DeploymentTeardownIdentity {
            owner,
            deployment_id,
            lifecycle_generation: generation,
            site_node_id: Some(node_id),
        }) {
            return Ok(CleanupDecision::AfterDurability);
        }
        "not_in_teardown"
    } else {
        let high_water = state
            .lifecycle_high_water(owner, deployment_id)?
            .unwrap_or(0)
            .max(declared_generation);
        if generation <= high_water {
            return Ok(CleanupDecision::WithoutEffect);
        }
        "future_generation"
    };
    debug!(
        event_kind = "manager_cleanup_evidence",
        outcome = "rejected",
        owner = ?owner,
        node_id,
        deployment_id,
        lifecycle_generation = generation,
        reason,
        "Manager rejected one worker cleanup evidence entry"
    );
    Ok(CleanupDecision::Rejected)
}

/// Makes every staged removal durable when any decision waits for it. Returns the deployments
/// now durably absent from a clean site, whose remaining manager records the owner may delete, or
/// `None` when the write failed and the evidence stays unacknowledged for a later report.
pub fn finalize_cleanup_decisions<'a>(
    teardowns: &mut DeploymentTeardownTransaction<'_>,
    owner: DeploymentOwner,
    node_id: &str,
    decisions: &[(&'a str, CleanupDecision)],
) -> Option<BTreeSet<&'a str>> {
    let finalized = decisions
        .iter()
        .filter(|(_, decision)| *decision == CleanupDecision::AfterDurability)
        .map(|(deployment_id, _)| *deployment_id)
        .collect::<BTreeSet<_>>();
    if finalized.is_empty() {
        return Some(finalized);
    }
    if let Err(error) = teardowns.make_durable() {
        warn!(event_kind = "manager_cleanup_evidence", outcome = "retryable", owner = ?owner, node_id, error = %error, "Manager could not make teardown finalization durable; cleanup evidence stays unacknowledged");
        return None;
    }
    Some(finalized)
}

/// Lifecycle generation and placement SiteNode of one owner's accepted declaration.
fn declared_deployment(
    teardowns: &DeploymentTeardownTransaction<'_>,
    owner: DeploymentOwner,
    deployment_id: &str,
) -> Option<(u64, Option<String>)> {
    match owner {
        DeploymentOwner::Model => teardowns
            .model_deployment(deployment_id)
            .map(|declaration| {
                (
                    declaration.lifecycle_generation,
                    declaration
                        .placement
                        .as_ref()
                        .map(|placement| placement.site_node_id.clone()),
                )
            }),
        DeploymentOwner::Application => {
            teardowns
                .application_deployment(deployment_id)
                .map(|declaration| {
                    (
                        declaration.lifecycle_generation,
                        declaration
                            .placement
                            .as_ref()
                            .map(|placement| placement.site_node_id.clone()),
                    )
                })
        }
    }
}
